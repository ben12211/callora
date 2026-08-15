import type { AgentConfig } from '../domain/models.js';
import { truncateTranscript, type BridgeLogger, type MessageChannel } from './bridge.js';
import type { CartesiaSocket } from './cartesia-connection.js';
import {
  STT_CLOSE_COMMAND,
  buildTtsCancel,
  buildTtsChunk,
  cartesiaLanguage,
} from './cartesia-protocol.js';
import { composeAgentInstructions } from './policy.js';
import {
  END_CALL_TOOL_NAME,
  buildTwilioClear,
  buildTwilioMedia,
  buildTwilioMark,
  parseJsonObject,
  readObject,
  readString,
} from './protocol.js';
import { streamChatCompletion, type ChatMessage, type ChatToolDefinition } from './text-llm.js';

/** Ceiling on waiting for the goodbye audio to drain before hanging up anyway. */
export const CARTESIA_DRAIN_TIMEOUT_MS = 5_000;

/** Cartesia drops an idle STT socket after ~3 minutes; silence keeps it alive. */
export const STT_KEEPALIVE_MS = 30_000;

type EndState = 'none' | 'draining' | 'terminating' | 'terminated';

/** The same hangup contract the speech-to-speech providers expose, as an LLM tool. */
const END_CALL_TOOL: ChatToolDefinition = {
  type: 'function',
  function: {
    name: END_CALL_TOOL_NAME,
    description:
      'End the phone call after saying a short goodbye. Use when the caller says goodbye or thanks you and needs nothing else, asks to hang up, when their request is complete, or after repeated unrelated or abusive turns.',
    parameters: {
      type: 'object',
      properties: { reason: { type: 'string', description: 'Why the call is ending.' } },
      required: ['reason'],
      additionalProperties: false,
    },
  },
};

export interface CartesiaBridgeOptions {
  twilio: MessageChannel;
  stt: CartesiaSocket;
  tts: CartesiaSocket;
  agent: AgentConfig;
  businessId: string;
  callSid: string;
  callId?: string | null;
  callerNumber?: string | null;
  ttsModel: string;
  voiceId: string;
  llm: { baseUrl: string; apiKey: string; model: string };
  logger: BridgeLogger;
  onIdentifiers?: (identifiers: { streamSid: string | null; sessionId: string | null }) => void;
  endCall?: (reason: string) => Promise<void>;
  /** Injectable so tests never reach a real LLM. */
  streamChat?: typeof streamChatCompletion;
}

/**
 * Bridges one Twilio Media Stream to a Cartesia STT -> text LLM -> Cartesia Sonic TTS
 * pipeline.
 *
 * Unlike the OpenAI Realtime and ElevenLabs bridges, no vendor owns the turn loop here:
 * this class does. It decides when the caller has finished a turn, runs the reasoning
 * step, and streams the reply into speech as it is generated rather than after it is
 * complete. The audio path is mu-law 8 kHz end to end, so nothing is transcoded — the
 * only conversion is base64, because Twilio wraps its mu-law in JSON and Cartesia STT
 * wants raw bytes.
 */
export class CartesiaBridge {
  private streamSid: string | null = null;
  private closed = false;
  private pendingMarks = 0;

  /** Conversation so far, seeded with the composed Callora policy. */
  private readonly history: ChatMessage[] = [];
  /** Caller speech accumulated from final STT deltas until the turn is dispatched. */
  private callerBuffer = '';
  /** Identifies the current assistant utterance; late chunks from older ones are dropped. */
  private activeContextId: string | null = null;
  private turnCounter = 0;
  /** Aborts an in-flight LLM turn when the caller barges in. */
  private turnAbort: AbortController | null = null;
  private agentSpeaking = false;
  /**
   * Contexts closed with `continue: false` whose `done` has not arrived yet.
   *
   * The LLM reports `end_call` the moment its stream finishes, which is typically before
   * Sonic has returned any of the goodbye. Draining on queued Twilio marks alone would
   * therefore hang up on the caller mid-farewell.
   */
  private readonly awaitingTtsDone = new Set<string>();

  private endState: EndState = 'none';
  private endReason: string | null = null;
  private endTimer: ReturnType<typeof setTimeout> | null = null;
  private keepaliveTimer: ReturnType<typeof setInterval> | null = null;

  public constructor(private readonly options: CartesiaBridgeOptions) {}

  public start(): void {
    const { twilio, stt, tts, agent, businessId, callSid, callerNumber, logger } = this.options;

    this.history.push({
      role: 'system',
      content: composeAgentInstructions({ agent, callerNumber }),
    });

    twilio.onMessage((raw) => this.handleTwilioMessage(raw));
    twilio.onClose(() => {
      logger.info({ businessId, callSid, streamSid: this.streamSid }, 'Twilio media stream closed');
      this.close('twilio-closed');
    });
    twilio.onError((error) => {
      logger.error({ businessId, callSid, error: error.message }, 'Twilio media stream error');
      this.close('twilio-error');
    });

    stt.onMessage((raw) => this.handleSttMessage(raw));
    stt.onClose(() => this.close('stt-closed'));
    stt.onError((error) => {
      logger.error({ businessId, callSid, error: error.message }, 'Cartesia STT error');
      this.close('stt-error');
    });

    tts.onMessage((raw) => this.handleTtsMessage(raw));
    tts.onClose(() => this.close('tts-closed'));
    tts.onError((error) => {
      logger.error({ businessId, callSid, error: error.message }, 'Cartesia TTS error');
      this.close('tts-error');
    });

    // The language and greeting are tenant configuration, not secrets, and are the first
    // things to check when a call comes out in the wrong language.
    logger.info(
      {
        ...this.logContext(),
        language: cartesiaLanguage(agent) ?? null,
        agentLanguage: agent.language,
        ttsModel: this.options.ttsModel,
        firstMessage: agent.greeting,
      },
      'Starting Cartesia pipeline',
    );

    this.keepaliveTimer = setInterval(() => this.sendSttKeepalive(), STT_KEEPALIVE_MS);
    this.keepaliveTimer.unref?.();
  }

  private logContext(): Record<string, unknown> {
    return {
      callId: this.options.callId ?? undefined,
      businessId: this.options.businessId,
      callSid: this.options.callSid,
      streamSid: this.streamSid ?? undefined,
    };
  }

  public close(reason: string): void {
    if (this.closed) {
      return;
    }
    this.closed = true;
    this.clearEndTimer();
    if (this.keepaliveTimer) {
      clearInterval(this.keepaliveTimer);
      this.keepaliveTimer = null;
    }
    this.turnAbort?.abort();

    this.options.logger.info(
      { ...this.logContext(), reason },
      'Realtime call bridge closed',
    );
    try {
      this.options.twilio.close();
    } finally {
      try {
        this.options.stt.sendText(STT_CLOSE_COMMAND);
      } catch {
        // The socket is already going away; closing it below is enough.
      }
      this.options.stt.close();
      this.options.tts.close();
    }
  }

  /** Cartesia disconnects an idle STT socket, and a silent caller is still a live call. */
  private sendSttKeepalive(): void {
    if (this.closed) {
      return;
    }
    // 100 ms of mu-law silence. 0xFF is digital silence in G.711 mu-law.
    this.options.stt.sendBinary(Buffer.alloc(800, 0xff));
  }

  private handleTwilioMessage(raw: string): void {
    if (this.closed) {
      return;
    }
    const message = parseJsonObject(raw);
    if (!message) {
      return;
    }

    switch (readString(message, 'event')) {
      case 'start': {
        const start = readObject(message, 'start');
        this.streamSid = readString(message, 'streamSid') ?? (start ? readString(start, 'streamSid') : null) ?? null;
        const startedCallSid = start ? readString(start, 'callSid') : undefined;
        if (startedCallSid && startedCallSid !== this.options.callSid) {
          this.options.logger.warn(
            { businessId: this.options.businessId, callSid: this.options.callSid },
            'Twilio stream CallSid does not match the authorized call; closing',
          );
          this.close('call-sid-mismatch');
          return;
        }
        this.options.logger.info({ ...this.logContext() }, 'Twilio media stream started');
        this.options.onIdentifiers?.({ streamSid: this.streamSid, sessionId: null });
        this.speakGreeting();
        return;
      }
      case 'media': {
        const media = readObject(message, 'media');
        const payload = media ? readString(media, 'payload') : undefined;
        if (payload) {
          // base64 -> raw mu-law bytes. Not a transcode: the samples are untouched.
          this.options.stt.sendBinary(Buffer.from(payload, 'base64'));
        }
        return;
      }
      case 'mark': {
        if (this.pendingMarks > 0) {
          this.pendingMarks -= 1;
        }
        if (this.pendingMarks === 0) {
          this.agentSpeaking = false;
        }
        this.terminateWhenAudioDrained();
        return;
      }
      case 'stop': {
        this.close('twilio-stop');
        return;
      }
      default:
        return;
    }
  }

  /** The tenant greeting, spoken as the first utterance. */
  private speakGreeting(): void {
    const greeting = this.options.agent.greeting.trim();
    if (!greeting) {
      this.options.logger.warn(this.logContext(), 'The business has no greeting configured');
      return;
    }
    const contextId = this.nextContext();
    this.sendTtsChunk(contextId, greeting, false);
    this.history.push({ role: 'assistant', content: greeting });
    this.logTranscript('AI', greeting);
  }

  private nextContext(): string {
    // Only the newest utterance gates a hangup. Audio from a superseded context is
    // dropped anyway, so a `done` that never arrives for one must not strand the call.
    this.awaitingTtsDone.clear();
    this.turnCounter += 1;
    const contextId = `${this.options.callSid}-${this.turnCounter}`;
    this.activeContextId = contextId;
    this.agentSpeaking = true;
    return contextId;
  }

  private sendTtsChunk(contextId: string, transcript: string, more: boolean): void {
    if (!more) {
      this.awaitingTtsDone.add(contextId);
    }
    this.options.tts.sendText(
      JSON.stringify(
        buildTtsChunk({
          model: this.options.ttsModel,
          voiceId: this.options.voiceId,
          contextId,
          transcript,
          language: cartesiaLanguage(this.options.agent),
          continue: more,
        }),
      ),
    );
  }

  private handleSttMessage(raw: string): void {
    if (this.closed) {
      return;
    }
    const message = parseJsonObject(raw);
    if (!message || readString(message, 'type') !== 'transcript') {
      return;
    }

    const text = readString(message, 'text') ?? '';
    const isFinal = message['is_final'] === true;

    if (!isFinal) {
      // A partial with actual words is the earliest reliable sign the caller has
      // started talking, which is what barge-in has to key on.
      if (text.trim() && this.agentSpeaking) {
        this.handleBargeIn();
      }
      return;
    }

    // Final transcripts are deltas, concatenated verbatim without added whitespace.
    this.callerBuffer += text;
    const utterance = this.callerBuffer.trim();
    if (!utterance) {
      return;
    }
    this.callerBuffer = '';
    this.logTranscript('USER', utterance);
    this.history.push({ role: 'user', content: utterance });
    void this.runTurn();
  }

  /**
   * Caller started talking over the agent.
   *
   * Cartesia's cancel only stops generations that have not started, so the in-flight one
   * keeps streaming chunks regardless. Abandoning the context id is what actually silences
   * the agent: later chunks tagged with it are dropped instead of forwarded.
   */
  private handleBargeIn(): void {
    const abandoned = this.activeContextId;
    this.activeContextId = null;
    this.agentSpeaking = false;

    if (abandoned) {
      this.options.tts.sendText(JSON.stringify(buildTtsCancel(abandoned)));
      // Its `done` may never arrive now, so it must not hold up a later hangup.
      this.awaitingTtsDone.delete(abandoned);
    }
    this.turnAbort?.abort();

    if (this.streamSid) {
      this.options.twilio.send(JSON.stringify(buildTwilioClear(this.streamSid)));
    }
    this.pendingMarks = 0;

    if (this.endState === 'draining') {
      this.terminate('interrupted-farewell');
      return;
    }
    this.options.logger.debug(this.logContext(), 'Caller barge-in; stopped the agent and resumed listening');
  }

  /** One reasoning turn, streamed into speech as it is produced. */
  private async runTurn(): Promise<void> {
    if (this.closed || this.endState !== 'none') {
      return;
    }

    const abort = new AbortController();
    this.turnAbort = abort;
    const contextId = this.nextContext();
    const streamChat = this.options.streamChat ?? streamChatCompletion;

    // Sonic synthesises per fragment, so pushing raw tokens would chop prosody. Holding
    // back to a clause boundary keeps speech natural while still starting long before
    // the reply is finished.
    let pending = '';
    const flush = (force: boolean): void => {
      if (abort.signal.aborted) {
        return;
      }
      const boundary = force ? pending.length : Math.max(...['. ', '! ', '? ', ', ', '\n', '׃', '، '].map((mark) => pending.lastIndexOf(mark) + mark.length));
      if (boundary <= 0) {
        return;
      }
      const ready = pending.slice(0, boundary);
      pending = pending.slice(boundary);
      if (ready.trim()) {
        this.sendTtsChunk(contextId, ready, true);
      }
    };

    let result;
    try {
      result = await streamChat({
        baseUrl: this.options.llm.baseUrl,
        apiKey: this.options.llm.apiKey,
        model: this.options.llm.model,
        messages: this.history,
        tools: [END_CALL_TOOL],
        signal: abort.signal,
        onTextDelta: (delta) => {
          pending += delta;
          flush(false);
        },
      });
    } catch (error) {
      this.options.logger.error(
        { ...this.logContext(), error: error instanceof Error ? error.message : 'unknown error' },
        'The reasoning turn failed',
      );
      return;
    } finally {
      if (this.turnAbort === abort) {
        this.turnAbort = null;
      }
    }

    if (abort.signal.aborted || result.aborted) {
      // Barged in: the context is already abandoned and its audio discarded.
      return;
    }

    if (pending) {
      flush(true);
    }
    // Closing the context tells Sonic the utterance is complete.
    this.sendTtsChunk(contextId, '', false);

    if (result.text.trim()) {
      this.history.push({ role: 'assistant', content: result.text });
      this.logTranscript('AI', result.text);
    }

    const endCall = result.toolCalls.find((call) => call.function.name === END_CALL_TOOL_NAME);
    if (endCall) {
      let reason = 'model_requested';
      const parsed = parseJsonObject(endCall.function.arguments);
      if (parsed) {
        reason = readString(parsed, 'reason') ?? reason;
      }
      this.requestEndCall(reason);
    }
  }

  private handleTtsMessage(raw: string): void {
    if (this.closed) {
      return;
    }
    const message = parseJsonObject(raw);
    if (!message) {
      return;
    }

    switch (readString(message, 'type')) {
      case 'chunk': {
        const contextId = readString(message, 'context_id');
        const data = readString(message, 'data');
        // Cancel does not stop an in-flight generation, so audio from an abandoned
        // context keeps arriving. Forwarding it would talk over the caller.
        if (!data || !this.streamSid || contextId !== this.activeContextId) {
          return;
        }
        this.options.twilio.send(JSON.stringify(buildTwilioMedia(this.streamSid, data)));
        this.options.twilio.send(JSON.stringify(buildTwilioMark(this.streamSid)));
        this.pendingMarks += 1;
        this.agentSpeaking = true;
        return;
      }
      case 'done': {
        const contextId = readString(message, 'context_id');
        if (contextId) {
          this.awaitingTtsDone.delete(contextId);
        }
        this.terminateWhenAudioDrained();
        return;
      }
      case 'error': {
        this.options.logger.error(
          {
            ...this.logContext(),
            code: readString(message, 'error_code'),
            reason: readString(message, 'message') ?? readString(message, 'title'),
          },
          'Cartesia TTS reported an error',
        );
        return;
      }
      default:
        return;
    }
  }

  private logTranscript(speaker: 'USER' | 'AI', transcript: string): void {
    const text = truncateTranscript(transcript);
    if (text) {
      this.options.logger.info(this.logContext(), `[conversation] ${speaker}: ${text}`);
    }
  }

  private requestEndCall(reason: string): void {
    if (this.closed || this.endState !== 'none') {
      return;
    }
    this.endState = 'draining';
    this.endReason = reason;
    this.options.logger.info(
      { businessId: this.options.businessId, callSid: this.options.callSid, reason },
      'Ending the call once the goodbye has played',
    );
    this.armEndTimer(CARTESIA_DRAIN_TIMEOUT_MS, 'farewell-drain-timeout');
    this.terminateWhenAudioDrained();
  }

  /** Safe to hang up only once Sonic has finished generating and Twilio has played it. */
  private terminateWhenAudioDrained(): void {
    if (this.endState === 'draining' && this.awaitingTtsDone.size === 0 && this.pendingMarks === 0) {
      this.terminate(this.endReason ?? 'end-call');
    }
  }

  private terminate(reason: string): void {
    if (this.endState === 'terminating' || this.endState === 'terminated') {
      return;
    }
    this.endState = 'terminating';
    this.clearEndTimer();

    const { businessId, callSid, logger, endCall } = this.options;
    const finish = (): void => {
      this.endState = 'terminated';
      this.close(`end-call:${reason}`);
    };

    if (!endCall) {
      finish();
      return;
    }

    endCall(reason).then(
      () => {
        logger.info({ businessId, callSid, reason }, 'Twilio call terminated');
        finish();
      },
      (error: unknown) => {
        logger.error(
          { businessId, callSid, reason, error: error instanceof Error ? error.message : 'unknown error' },
          'Failed to terminate the Twilio call; closing the media stream instead',
        );
        finish();
      },
    );
  }

  private armEndTimer(delayMs: number, reason: string): void {
    this.clearEndTimer();
    this.endTimer = setTimeout(() => {
      this.endTimer = null;
      this.options.logger.warn(
        { businessId: this.options.businessId, callSid: this.options.callSid, reason },
        'Goodbye audio did not complete in time; hanging up anyway',
      );
      this.terminate(reason);
    }, delayMs);
    this.endTimer.unref?.();
  }

  private clearEndTimer(): void {
    if (this.endTimer) {
      clearTimeout(this.endTimer);
      this.endTimer = null;
    }
  }
}

import type { AgentConfig } from '../domain/models.js';
import type { CallMetrics } from '../platform/metrics.js';
import {
  HangupSequence,
  SilenceWatchdog,
  truncateTranscript,
  type BridgeLogger,
  type MessageChannel,
  type SilenceOptions,
} from './call-leg.js';
import {
  ELEVENLABS_AUDIO_FORMAT,
  ELEVENLABS_END_CALL_TOOL_NAME,
  buildClientToolResult,
  buildConversationInitiation,
  buildPong,
  buildUserAudioChunk,
  resolveConversationOverrides,
} from './elevenlabs-protocol.js';
import {
  buildTwilioClear,
  buildTwilioMedia,
  buildTwilioMark,
  parseJsonObject,
  readObject,
  readString,
} from './protocol.js';

/** Ceiling on waiting for the goodbye audio to drain before hanging up anyway. */
export const ELEVENLABS_DRAIN_TIMEOUT_MS = 5_000;

export interface ElevenLabsBridgeOptions {
  twilio: MessageChannel;
  elevenlabs: MessageChannel;
  agent: AgentConfig;
  businessId: string;
  callSid: string;
  /** Callora's own call record id, when the call row already exists. */
  callId?: string | null;
  callerNumber?: string | null;
  /**
   * ElevenLabs voice id chosen for this business in the control plane. Absent keeps the
   * voice configured on the ElevenLabs agent itself.
   */
  voiceId?: string;
  logger: BridgeLogger;
  /** Called once the Twilio stream and, when known, the ElevenLabs conversation are identified. */
  onIdentifiers?: (identifiers: { streamSid: string | null; sessionId: string | null }) => void;
  /**
   * Terminates the underlying Twilio call. The bridge supplies no identifier: the
   * caller closes over the CallSid the stream was authorized for.
   */
  endCall?: (reason: string) => Promise<void>;
  silence?: SilenceOptions;
  /** Call-quality counters; absent simply records nothing. */
  metrics?: CallMetrics;
}

function readNumber(source: Record<string, unknown>, key: string): number | undefined {
  const value = source[key];
  return typeof value === 'number' && Number.isFinite(value) ? value : undefined;
}

/**
 * Bridges exactly one Twilio Media Stream to exactly one ElevenLabs Agents conversation.
 *
 * ElevenLabs runs turn detection, interruption, and the LLM turn loop on its own side, so
 * this bridge is deliberately thinner than the OpenAI one: it forwards audio both ways,
 * answers keepalives, mirrors ElevenLabs' `interruption` event onto Twilio's buffer, and
 * owns only the hangup sequence, which has to happen through the Twilio REST API.
 *
 * The bridge owns no global state: every call gets its own instance, and closing either
 * side tears down the other exactly once.
 */
export class ElevenLabsBridge {
  private streamSid: string | null = null;
  private conversationId: string | null = null;
  private closed = false;

  /** Agent audio chunks handed to Twilio but not yet acknowledged by a mark. */
  private pendingMarks = 0;

  /** When the Twilio stream opened, and whether the caller has heard anything yet. */
  private streamOpenedAt: number | null = null;
  private reportedFirstAudio = false;

  /** Shared with every other provider bridge: see `call-leg.ts`. */
  private readonly hangup: HangupSequence;
  private readonly silence: SilenceWatchdog;

  public constructor(private readonly options: ElevenLabsBridgeOptions) {
    this.hangup = new HangupSequence({
      businessId: options.businessId,
      callSid: options.callSid,
      logger: options.logger,
      endCall: options.endCall,
      audioDrained: () => this.pendingMarks === 0,
      onFinished: (reason) => this.close(`end-call:${reason}`),
      drainTimeoutMs: ELEVENLABS_DRAIN_TIMEOUT_MS,
    });
    // ElevenLabs gives Callora no supported way to make the agent speak unprompted, so
    // this runs a single stage: a caller who goes quiet is hung up on rather than left
    // holding a Twilio leg and a provider session open until the hour-long ceiling.
    this.silence = new SilenceWatchdog({
      ...options.silence,
      armed: () => !this.closed && !this.hangup.active && this.streamSid !== null,
      agentSpeaking: () => this.pendingMarks > 0,
      onHangup: () => {
        options.logger.info(
          { businessId: options.businessId, callSid: options.callSid },
          'Caller silent; ending the call',
        );
        this.requestEndCall('caller_silent');
      },
    });
  }

  public start(): void {
    const { twilio, elevenlabs, agent, businessId, callSid, callerNumber, voiceId, logger } = this.options;

    twilio.onMessage((raw) => this.handleTwilioMessage(raw));
    twilio.onClose(() => {
      logger.info({ businessId, callSid, streamSid: this.streamSid }, 'Twilio media stream closed');
      this.close('twilio-closed');
    });
    twilio.onError((error) => {
      logger.error({ businessId, callSid, error: error.message }, 'Twilio media stream error');
      this.close('twilio-error');
    });

    elevenlabs.onMessage((raw) => this.handleElevenLabsMessage(raw));
    elevenlabs.onClose(() => {
      logger.info({ businessId, callSid, conversationId: this.conversationId }, 'ElevenLabs conversation closed');
      this.close('elevenlabs-closed');
    });
    elevenlabs.onError((error) => {
      logger.error({ businessId, callSid, error: error.message }, 'ElevenLabs conversation error');
      this.close('elevenlabs-error');
    });

    const overrides = resolveConversationOverrides({ agent, callerNumber, voiceId });

    // The language and greeting are tenant configuration, not secrets, and they are the
    // two values worth seeing on every call: if a Hebrew agent answers in English, this
    // line says whether Callora sent the wrong override or ElevenLabs ignored the right one.
    logger.info(
      {
        ...this.logContext(),
        language: overrides.language ?? null,
        agentLanguage: agent.language,
        firstMessage: overrides.firstMessage,
        voiceId: overrides.voiceId ?? null,
      },
      'Sending ElevenLabs conversation overrides',
    );

    // Without a greeting there is no first_message to override, so the agent would open
    // with whatever is configured on the ElevenLabs side — in practice its English default.
    if (!overrides.firstMessage) {
      logger.warn(
        this.logContext(),
        'The business has no greeting configured; ElevenLabs will use its own first message',
      );
    }

    // Debug level only: the composed policy is long and is meant for verifying locally
    // what the agent actually received, not for production log volume.
    logger.debug({ ...this.logContext(), instructions: overrides.prompt }, 'Composed realtime agent instructions');

    elevenlabs.send(JSON.stringify(buildConversationInitiation({ agent, callerNumber, voiceId })));
  }

  /** Identifiers attached to every conversation log line. */
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
    this.silence.stop();
    this.hangup.dispose();
    this.options.logger.info(
      {
        businessId: this.options.businessId,
        callSid: this.options.callSid,
        streamSid: this.streamSid,
        conversationId: this.conversationId,
        reason,
      },
      'Realtime call bridge closed',
    );
    try {
      this.options.twilio.close();
    } finally {
      this.options.elevenlabs.close();
    }
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
        this.options.logger.info(
          { businessId: this.options.businessId, callSid: this.options.callSid, streamSid: this.streamSid },
          'Twilio media stream started',
        );
        this.streamOpenedAt = Date.now();
        this.options.onIdentifiers?.({ streamSid: this.streamSid, sessionId: this.conversationId });
        this.silence.restart();
        return;
      }
      case 'media': {
        const media = readObject(message, 'media');
        const payload = media ? readString(media, 'payload') : undefined;
        if (payload) {
          this.options.elevenlabs.send(JSON.stringify(buildUserAudioChunk(payload)));
        }
        return;
      }
      case 'mark': {
        if (this.pendingMarks > 0) {
          this.pendingMarks -= 1;
        }
        // A mark means Twilio finished playing that chunk to the caller, so this is the
        // only reliable signal that the goodbye was actually heard.
        this.hangup.terminateWhenDrained();
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

  private handleElevenLabsMessage(raw: string): void {
    // Closing a WebSocket is asynchronous, so frames already in flight still arrive
    // afterwards. Once the bridge is closed — in particular after refusing a session
    // whose audio format Twilio cannot play — none of them may reach the caller.
    if (this.closed) {
      return;
    }
    const message = parseJsonObject(raw);
    if (!message) {
      return;
    }

    switch (readString(message, 'type')) {
      case 'conversation_initiation_metadata': {
        const metadata = readObject(message, 'conversation_initiation_metadata_event');
        this.conversationId = (metadata ? readString(metadata, 'conversation_id') : undefined) ?? null;
        const outputFormat = metadata ? readString(metadata, 'agent_output_audio_format') : undefined;
        const inputFormat = metadata ? readString(metadata, 'user_input_audio_format') : undefined;

        // Twilio only speaks mu-law 8 kHz. Anything else would need transcoding this
        // bridge deliberately does not do, so it is a configuration error worth shouting
        // about rather than silently producing noise on the call.
        if (outputFormat !== ELEVENLABS_AUDIO_FORMAT || inputFormat !== ELEVENLABS_AUDIO_FORMAT) {
          this.options.logger.error(
            { ...this.logContext(), outputFormat, inputFormat, expected: ELEVENLABS_AUDIO_FORMAT },
            'ElevenLabs agent is not configured for mu-law 8 kHz audio; closing the call',
          );
          this.close('audio-format-mismatch');
          return;
        }

        this.options.logger.info(
          {
            businessId: this.options.businessId,
            callSid: this.options.callSid,
            conversationId: this.conversationId,
          },
          'ElevenLabs conversation started',
        );
        this.options.onIdentifiers?.({ streamSid: this.streamSid, sessionId: this.conversationId });
        return;
      }
      case 'audio': {
        const audio = readObject(message, 'audio_event');
        const payload = audio ? readString(audio, 'audio_base_64') : undefined;
        if (!payload || !this.streamSid) {
          return;
        }
        this.reportFirstAudio();
        this.options.twilio.send(JSON.stringify(buildTwilioMedia(this.streamSid, payload)));
        this.options.twilio.send(JSON.stringify(buildTwilioMark(this.streamSid)));
        this.pendingMarks += 1;
        return;
      }
      case 'interruption': {
        // ElevenLabs detected the caller talking over the agent and has already stopped
        // generating. Everything Twilio still has buffered is audio the agent has
        // abandoned, so dropping it is what actually stops the voice mid-word.
        this.handleInterruption();
        return;
      }
      case 'ping': {
        const ping = readObject(message, 'ping_event');
        const eventId = ping ? readNumber(ping, 'event_id') : undefined;
        if (eventId !== undefined) {
          this.options.elevenlabs.send(JSON.stringify(buildPong(eventId)));
        }
        return;
      }
      case 'user_transcript': {
        const event = readObject(message, 'user_transcription_event');
        // ElevenLabs runs turn detection itself, so a finished caller transcript is the
        // clearest signal Callora gets that the caller is still on the line.
        this.silence.reset();
        this.logTranscript('USER', event ? readString(event, 'user_transcript') : undefined);
        return;
      }
      case 'agent_response': {
        const event = readObject(message, 'agent_response_event');
        this.silence.restart();
        this.logTranscript('AI', event ? readString(event, 'agent_response') : undefined);
        return;
      }
      case 'client_tool_call': {
        const call = readObject(message, 'client_tool_call');
        this.handleClientToolCall(
          call ? readString(call, 'tool_name') : undefined,
          call ? readString(call, 'tool_call_id') : undefined,
          call ? readObject(call, 'parameters') : undefined,
        );
        return;
      }
      case 'error':
      case 'conversation_initiation_client_data_error': {
        // ElevenLabs refuses an override whose field is not enabled under the agent's
        // Security tab, which leaves the agent speaking its own configured first message
        // and language. Swallowing that made a Hebrew agent silently answer in English.
        const error = readObject(message, 'error');
        this.options.logger.error(
          {
            ...this.logContext(),
            code: error ? readString(error, 'code') : undefined,
            reason: (error ? readString(error, 'message') : undefined) ?? readString(message, 'message'),
          },
          'ElevenLabs rejected the conversation configuration; check that the prompt, first message, and language overrides are enabled on the agent',
        );
        return;
      }
      default: {
        // Never drop an event silently again: the type alone is enough to notice a
        // rejection or a protocol change, and it carries no payload or credential.
        this.options.logger.debug(
          { ...this.logContext(), event: readString(message, 'type') },
          'Ignoring an unhandled ElevenLabs event',
        );
        return;
      }
    }
  }

  /**
   * The caller has now heard the agent for the first time. Everything before this is
   * silence on the line, which is the latency that actually matters on a phone call.
   */
  private reportFirstAudio(): void {
    if (this.reportedFirstAudio || this.streamOpenedAt === null) {
      return;
    }
    this.reportedFirstAudio = true;
    this.options.metrics?.firstAudio('elevenlabs', Date.now() - this.streamOpenedAt);
  }

  /** Caller started speaking: drop whatever Twilio still has queued. */
  private handleInterruption(): void {
    this.options.metrics?.bargeIn('elevenlabs');
    this.silence.reset();
    if (this.hangup.closing) {
      // The caller talked over the goodbye. Stop the audio and hang up now rather than
      // waiting for marks that will never arrive.
      if (this.streamSid) {
        this.options.twilio.send(JSON.stringify(buildTwilioClear(this.streamSid)));
      }
      this.pendingMarks = 0;
      this.hangup.terminate('interrupted-farewell');
      return;
    }
    if (!this.streamSid || this.pendingMarks === 0) {
      return;
    }
    this.options.twilio.send(JSON.stringify(buildTwilioClear(this.streamSid)));
    this.pendingMarks = 0;
  }

  /**
   * One line per completed turn. Only the transcript text is logged: never audio
   * payloads, credentials, or the raw event.
   */
  private logTranscript(speaker: 'USER' | 'AI', transcript: string | undefined): void {
    const text = truncateTranscript(transcript ?? '');
    if (!text) {
      return;
    }
    this.options.logger.info(this.logContext(), `[conversation] ${speaker}: ${text}`);
  }

  private handleClientToolCall(
    name: string | undefined,
    toolCallId: string | undefined,
    parameters: Record<string, unknown> | undefined,
  ): void {
    if (name !== ELEVENLABS_END_CALL_TOOL_NAME) {
      if (name) {
        this.options.logger.warn(
          { businessId: this.options.businessId, callSid: this.options.callSid, tool: name },
          'Ignoring an unknown ElevenLabs client tool call',
        );
        if (toolCallId) {
          this.options.elevenlabs.send(
            JSON.stringify(buildClientToolResult(toolCallId, `Unknown tool: ${name}`, true)),
          );
        }
      }
      return;
    }

    // The reason is advisory only; the server hangs up the call this stream was
    // authorized for, never one named by the agent.
    const reason = (parameters ? readString(parameters, 'reason') : undefined) ?? 'model_requested';
    if (toolCallId) {
      this.options.elevenlabs.send(
        JSON.stringify(buildClientToolResult(toolCallId, 'The call is ending. Say nothing further.')),
      );
    }
    this.requestEndCall(reason);
  }

  /**
   * Starts the hangup sequence: let whatever goodbye the agent already spoke finish
   * reaching the caller, then terminate. Repeated requests are ignored, so a retried
   * tool call never queues a second hangup.
   */
  private requestEndCall(reason: string): void {
    if (this.closed || this.hangup.active) {
      return;
    }

    this.silence.stop();
    this.options.logger.info(
      { businessId: this.options.businessId, callSid: this.options.callSid, reason },
      'Ending the call once the goodbye has played',
    );
    this.hangup.beginDraining(reason);
  }
}

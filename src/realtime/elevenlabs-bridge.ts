import type { AgentConfig } from '../domain/models.js';
import { truncateTranscript, type BridgeLogger, type MessageChannel } from './bridge.js';
import {
  ELEVENLABS_AUDIO_FORMAT,
  ELEVENLABS_END_CALL_TOOL_NAME,
  buildClientToolResult,
  buildConversationInitiation,
  buildPong,
  buildUserAudioChunk,
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

/** Progress of a hangup, from the agent's tool call to the terminated Twilio call. */
type EndState = 'none' | 'draining' | 'terminating' | 'terminated';

export interface ElevenLabsBridgeOptions {
  twilio: MessageChannel;
  elevenlabs: MessageChannel;
  agent: AgentConfig;
  businessId: string;
  callSid: string;
  /** Callora's own call record id, when the call row already exists. */
  callId?: string | null;
  callerNumber?: string | null;
  logger: BridgeLogger;
  /** Called once the Twilio stream and, when known, the ElevenLabs conversation are identified. */
  onIdentifiers?: (identifiers: { streamSid: string | null; sessionId: string | null }) => void;
  /**
   * Terminates the underlying Twilio call. The bridge supplies no identifier: the
   * caller closes over the CallSid the stream was authorized for.
   */
  endCall?: (reason: string) => Promise<void>;
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

  private endState: EndState = 'none';
  private endReason: string | null = null;
  private endTimer: ReturnType<typeof setTimeout> | null = null;

  public constructor(private readonly options: ElevenLabsBridgeOptions) {}

  public start(): void {
    const { twilio, elevenlabs, agent, businessId, callSid, callerNumber, logger } = this.options;

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

    // Debug level only: the composed policy is long and is meant for verifying locally
    // what the agent actually received, not for production log volume.
    const initiation = buildConversationInitiation({ agent, callerNumber });
    logger.debug({ ...this.logContext() }, 'Sending ElevenLabs conversation overrides');
    elevenlabs.send(JSON.stringify(initiation));
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
    this.clearEndTimer();
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
        this.options.onIdentifiers?.({ streamSid: this.streamSid, sessionId: this.conversationId });
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

  private handleElevenLabsMessage(raw: string): void {
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
        this.logTranscript('USER', event ? readString(event, 'user_transcript') : undefined);
        return;
      }
      case 'agent_response': {
        const event = readObject(message, 'agent_response_event');
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
      default:
        return;
    }
  }

  /** Caller started speaking: drop whatever Twilio still has queued. */
  private handleInterruption(): void {
    if (this.endState === 'draining') {
      // The caller talked over the goodbye. Stop the audio and hang up now rather than
      // waiting for marks that will never arrive.
      if (this.streamSid) {
        this.options.twilio.send(JSON.stringify(buildTwilioClear(this.streamSid)));
      }
      this.pendingMarks = 0;
      this.terminate('interrupted-farewell');
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
    if (this.closed || this.endState !== 'none') {
      return;
    }

    this.endState = 'draining';
    this.endReason = reason;
    this.options.logger.info(
      { businessId: this.options.businessId, callSid: this.options.callSid, reason },
      'Ending the call once the goodbye has played',
    );
    this.armEndTimer(ELEVENLABS_DRAIN_TIMEOUT_MS, 'farewell-drain-timeout');
    this.terminateWhenAudioDrained();
  }

  private terminateWhenAudioDrained(): void {
    if (this.endState === 'draining' && this.pendingMarks === 0) {
      this.terminate(this.endReason ?? 'end-call');
    }
  }

  /** Idempotent: only the first call reaches Twilio, and only once. */
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
        // Hanging up the media stream still drops a <Connect><Stream> call, so a failed
        // REST hangup degrades rather than stranding the caller.
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

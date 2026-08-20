import type { AgentConfig } from '../domain/models.js';
import type { CallMetrics } from '../platform/metrics.js';
import { composeAgentInstructions } from './policy.js';
import {
  END_CALL_TOOL_NAME,
  buildAudioAppend,
  buildEndCallToolOutput,
  buildFarewellResponse,
  buildGreetingResponse,
  buildResponseCancel,
  buildSessionUpdate,
  buildStillThereResponse,
  buildTruncate,
  buildTwilioClear,
  buildTwilioMark,
  buildTwilioMedia,
  parseJsonObject,
  readObject,
  readObjectArray,
  readString,
} from './protocol.js';

import {
  DEFAULT_SILENCE_HANGUP_MS,
  DEFAULT_SILENCE_PROMPT_MS,
  FAREWELL_DRAIN_TIMEOUT_MS,
  FAREWELL_TIMEOUT_MS,
  HangupSequence,
  MAX_LOGGED_TRANSCRIPT_CHARS,
  SilenceWatchdog,
  truncateTranscript,
  type BridgeLogger,
  type EndState,
  type MessageChannel,
  type SilenceOptions,
} from './call-leg.js';

// Re-exported so existing importers keep one entry point for the bridge vocabulary.
export {
  DEFAULT_SILENCE_HANGUP_MS,
  DEFAULT_SILENCE_PROMPT_MS,
  FAREWELL_DRAIN_TIMEOUT_MS,
  FAREWELL_TIMEOUT_MS,
  MAX_LOGGED_TRANSCRIPT_CHARS,
  truncateTranscript,
};
export type { BridgeLogger, EndState, MessageChannel, SilenceOptions };

export interface BridgeOptions {
  twilio: MessageChannel;
  openai: MessageChannel;
  agent: AgentConfig;
  businessId: string;
  callSid: string;
  /** Callora's own call record id, when the call row already exists. */
  callId?: string | null;
  callerNumber?: string | null;
  /** Transcription model for caller audio; logging only. */
  transcriptionModel?: string;
  logger: BridgeLogger;
  /**
   * Called once the Twilio stream and, when known, the OpenAI session are identified.
   * `sessionId` rather than `openaiSessionId`, so all three bridges report one shape.
   */
  onIdentifiers?: (identifiers: { streamSid: string | null; sessionId: string | null }) => void;
  /**
   * Terminates the underlying Twilio call. The bridge supplies no identifier: the
   * caller closes over the CallSid the stream was authorized for.
   */
  endCall?: (reason: string) => Promise<void>;
  silence?: SilenceOptions;
  /** Call-quality counters; absent simply records nothing. */
  metrics?: CallMetrics;
  /**
   * One completed turn of the conversation. Absent keeps the previous behaviour of
   * logging only; the bridge never waits on it and never fails a call because of it.
   */
  onTranscript?: (turn: { speaker: 'caller' | 'agent'; content: string }) => void;
}

/**
 * Bridges exactly one Twilio Media Stream to exactly one OpenAI Realtime session.
 *
 * The bridge owns no global state: every call gets its own instance, and closing either
 * side tears down the other exactly once.
 */
export class MediaStreamBridge {
  private streamSid: string | null = null;
  private openaiSessionId: string | null = null;
  private greeted = false;
  private closed = false;

  /** Timestamp (ms into the stream) of the newest inbound media frame. */
  private latestMediaTimestamp = 0;
  /** Stream timestamp at which the current assistant response started playing. */
  private responseStartTimestamp: number | null = null;
  private lastAssistantItemId: string | null = null;
  /** Assistant audio chunks handed to Twilio but not yet acknowledged by a mark. */
  private pendingMarks = 0;

  /** True between `response.created` and `response.done`; a second response.create would be rejected. */
  private responseActive = false;
  /** True once this response has been cancelled by a barge-in, so it is cancelled only once. */
  private responseCancelled = false;
  /** Whether the in-flight response has produced any audio for the caller. */
  private assistantAudioInResponse = false;
  /** True once a goodbye has been asked for and is still being generated. */
  private farewellPending = false;
  /** `call_id`s of `end_call` tool calls already answered, so retries stay idempotent. */
  private readonly handledEndCallIds = new Set<string>();

  /** When the Twilio stream opened, and whether the caller has heard anything yet. */
  private streamOpenedAt: number | null = null;
  private reportedFirstAudio = false;

  /** Shared with every other provider bridge: see `call-leg.ts`. */
  private readonly hangup: HangupSequence;
  private readonly silence: SilenceWatchdog;

  public constructor(private readonly options: BridgeOptions) {
    this.hangup = new HangupSequence({
      businessId: options.businessId,
      callSid: options.callSid,
      logger: options.logger,
      endCall: options.endCall,
      audioDrained: () => this.pendingMarks === 0,
      onFinished: (reason) => this.close(`end-call:${reason}`),
    });
    this.silence = new SilenceWatchdog({
      ...options.silence,
      armed: () => !this.closed && !this.hangup.active && this.streamSid !== null,
      agentSpeaking: () => this.pendingMarks > 0 || this.responseStartTimestamp !== null,
      onPrompt: () => {
        options.logger.info(
          { businessId: options.businessId, callSid: options.callSid },
          'Caller silent; asking whether they are still on the line',
        );
        options.openai.send(JSON.stringify(buildStillThereResponse()));
      },
      onHangup: () => this.requestEndCall('caller_silent'),
    });
  }

  public start(): void {
    const { twilio, openai, agent, businessId, callSid, callerNumber, logger } = this.options;

    twilio.onMessage((raw) => this.handleTwilioMessage(raw));
    twilio.onClose(() => {
      logger.info({ businessId, callSid, streamSid: this.streamSid }, 'Twilio media stream closed');
      this.close('twilio-closed');
    });
    twilio.onError((error) => {
      logger.error({ businessId, callSid, error: error.message }, 'Twilio media stream error');
      this.close('twilio-error');
    });

    openai.onMessage((raw) => this.handleOpenAiMessage(raw));
    openai.onClose(() => {
      logger.info({ businessId, callSid, openaiSessionId: this.openaiSessionId }, 'OpenAI realtime connection closed');
      this.close('openai-closed');
    });
    openai.onError((error) => {
      logger.error({ businessId, callSid, error: error.message }, 'OpenAI realtime connection error');
      this.close('openai-error');
    });

    const sessionUpdate = buildSessionUpdate({
      agent,
      callerNumber,
      transcriptionModel: this.options.transcriptionModel,
    });

    // Debug level only: the composed policy is long and is meant for verifying locally
    // what the model actually received, not for production log volume.
    logger.debug(
      {
        ...this.logContext(),
        instructions: composeAgentInstructions({ agent, callerNumber }),
      },
      'Composed realtime agent instructions',
    );

    openai.send(JSON.stringify(sessionUpdate));
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
        openaiSessionId: this.openaiSessionId,
        reason,
      },
      'Realtime call bridge closed',
    );
    try {
      this.options.twilio.close();
    } finally {
      this.options.openai.close();
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
        this.streamOpenedAt = Date.now();
        this.options.onIdentifiers?.({ streamSid: this.streamSid, sessionId: this.openaiSessionId });
        this.silence.restart();
        return;
      }
      case 'media': {
        const media = readObject(message, 'media');
        const payload = media ? readString(media, 'payload') : undefined;
        const timestamp = media ? Number(media['timestamp']) : Number.NaN;
        if (Number.isFinite(timestamp)) {
          this.latestMediaTimestamp = timestamp;
        }
        if (payload) {
          this.options.openai.send(JSON.stringify(buildAudioAppend(payload)));
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

  private handleOpenAiMessage(raw: string): void {
    const message = parseJsonObject(raw);
    if (!message) {
      return;
    }
    const type = readString(message, 'type');

    switch (type) {
      case 'session.created':
      case 'session.updated': {
        const session = readObject(message, 'session');
        this.openaiSessionId = (session ? readString(session, 'id') : undefined) ?? this.openaiSessionId;
        if (type === 'session.created') {
          this.options.logger.info(
            {
              businessId: this.options.businessId,
              callSid: this.options.callSid,
              openaiSessionId: this.openaiSessionId,
              model: this.options.agent.realtimeModel,
            },
            'OpenAI realtime session created',
          );
        }
        this.options.onIdentifiers?.({ streamSid: this.streamSid, sessionId: this.openaiSessionId });
        if (!this.greeted) {
          this.greeted = true;
          this.options.openai.send(JSON.stringify(buildGreetingResponse(this.options.agent)));
        }
        return;
      }
      case 'response.created': {
        this.responseActive = true;
        this.responseCancelled = false;
        this.assistantAudioInResponse = false;
        return;
      }
      case 'response.output_audio.delta':
      case 'response.audio.delta': {
        const delta = readString(message, 'delta');
        if (!delta || !this.streamSid) {
          return;
        }
        this.assistantAudioInResponse = true;
        this.reportFirstAudio();
        if (this.responseStartTimestamp === null) {
          this.responseStartTimestamp = this.latestMediaTimestamp;
        }
        this.lastAssistantItemId = readString(message, 'item_id') ?? this.lastAssistantItemId;
        this.options.twilio.send(JSON.stringify(buildTwilioMedia(this.streamSid, delta)));
        this.options.twilio.send(JSON.stringify(buildTwilioMark(this.streamSid)));
        this.pendingMarks += 1;
        return;
      }
      case 'conversation.item.input_audio_transcription.completed': {
        this.logTranscript('USER', readString(message, 'transcript'));
        return;
      }
      case 'conversation.item.input_audio_transcription.failed': {
        this.options.logger.warn(this.logContext(), 'Caller audio transcription failed');
        return;
      }
      case 'response.output_audio_transcript.done':
      case 'response.audio_transcript.done': {
        this.logTranscript('AI', readString(message, 'transcript'));
        return;
      }
      case 'input_audio_buffer.speech_started': {
        // The caller is talking, so the silence escalation starts over from scratch.
        this.silence.reset();
        this.handleBargeIn();
        return;
      }
      case 'input_audio_buffer.speech_stopped': {
        this.silence.restart();
        return;
      }
      case 'response.function_call_arguments.done': {
        this.handleFunctionCall(readString(message, 'name'), readString(message, 'call_id'), message, false);
        return;
      }
      case 'response.done': {
        this.responseStartTimestamp = null;
        this.lastAssistantItemId = null;
        this.responseActive = false;
        this.responseCancelled = false;
        const spokeInResponse = this.assistantAudioInResponse;
        this.assistantAudioInResponse = false;

        // A hangup requested during this response is driven by the completion of it.
        this.advanceFarewell(spokeInResponse);

        // Some responses report the function call only in the completed response body.
        const response = readObject(message, 'response');
        for (const item of response ? readObjectArray(response, 'output') : []) {
          if (readString(item, 'type') === 'function_call') {
            this.handleFunctionCall(readString(item, 'name'), readString(item, 'call_id'), item, spokeInResponse);
          }
        }

        if (!this.hangup.active) {
          this.silence.restart();
        }
        return;
      }
      case 'error': {
        const error = readObject(message, 'error');
        this.options.logger.error(
          {
            businessId: this.options.businessId,
            callSid: this.options.callSid,
            openaiSessionId: this.openaiSessionId,
            code: error ? readString(error, 'code') : undefined,
            message: error ? readString(error, 'message') : undefined,
          },
          'OpenAI realtime error',
        );
        return;
      }
      default:
        return;
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
    this.options.metrics?.firstAudio('openai', Date.now() - this.streamOpenedAt);
  }

  /** Caller started speaking: drop queued assistant audio and rewind the model's item. */
  private handleBargeIn(): void {
    this.options.metrics?.bargeIn('openai');
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
    // An active response counts as speaking even before its first audio reaches Twilio:
    // waiting for queued marks is what used to let the agent finish a whole sentence
    // over the caller.
    if (!this.streamSid || (!this.responseActive && this.pendingMarks === 0)) {
      return;
    }

    // Stop generating first, so no further audio is produced for a turn the caller
    // has already talked over.
    if (this.responseActive && !this.responseCancelled) {
      this.responseCancelled = true;
      this.options.openai.send(JSON.stringify(buildResponseCancel()));
    }

    // Rewind the model's own record of what the caller actually heard, so the rest of
    // the conversation stays consistent with the truncated audio.
    if (this.lastAssistantItemId && this.responseStartTimestamp !== null) {
      const elapsedMs = this.latestMediaTimestamp - this.responseStartTimestamp;
      this.options.openai.send(JSON.stringify(buildTruncate(this.lastAssistantItemId, elapsedMs)));
    }

    // Drop whatever Twilio still has buffered; without this the caller keeps hearing
    // audio that the model has already stopped producing.
    this.options.twilio.send(JSON.stringify(buildTwilioClear(this.streamSid)));

    this.pendingMarks = 0;
    this.lastAssistantItemId = null;
    this.responseStartTimestamp = null;
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
    this.options.onTranscript?.({ speaker: speaker === 'USER' ? 'caller' : 'agent', content: text });
  }

  private handleFunctionCall(
    name: string | undefined,
    callId: string | undefined,
    source: Record<string, unknown>,
    alreadySaidGoodbye: boolean,
  ): void {
    if (name !== END_CALL_TOOL_NAME) {
      if (name) {
        this.options.logger.warn(
          { businessId: this.options.businessId, callSid: this.options.callSid, tool: name },
          'Ignoring an unknown realtime tool call',
        );
      }
      return;
    }
    if (callId && this.handledEndCallIds.has(callId)) {
      return;
    }
    if (callId) {
      this.handledEndCallIds.add(callId);
    }

    const args = parseJsonObject(readString(source, 'arguments') ?? '') ?? {};
    this.requestEndCall(readString(args, 'reason') ?? 'model_requested', callId, alreadySaidGoodbye);
  }

  /**
   * Starts the hangup sequence: acknowledge the tool call, let the model speak one
   * goodbye, and terminate once that audio has actually reached the caller. Repeated
   * requests only re-acknowledge; they never queue a second goodbye or a second hangup.
   */
  private requestEndCall(reason: string, callId?: string, alreadySaidGoodbye = false): void {
    if (this.closed) {
      return;
    }

    if (this.hangup.active) {
      if (callId) {
        this.options.openai.send(JSON.stringify(buildEndCallToolOutput(callId, true)));
      }
      this.options.logger.info(
        {
          businessId: this.options.businessId,
          callSid: this.options.callSid,
          reason,
          state: this.hangup.current,
        },
        'Ignoring a duplicate end_call request',
      );
      return;
    }

    this.silence.stop();
    this.options.logger.info(
      { businessId: this.options.businessId, callSid: this.options.callSid, reason },
      'Ending the call after a short goodbye',
    );

    if (callId) {
      this.options.openai.send(JSON.stringify(buildEndCallToolOutput(callId, false)));
    }
    this.hangup.enterFarewell(reason);

    if (alreadySaidGoodbye) {
      // The model said its goodbye in the turn that called the tool; asking for another
      // would just repeat it, so wait for that audio to finish playing.
      this.hangup.beginDraining();
      return;
    }
    if (!this.responseActive) {
      this.requestFarewellTurn();
    }
    // Otherwise the in-flight response drives it: see advanceFarewell().
  }

  /** Asks the model for exactly one closing line. */
  private requestFarewellTurn(): void {
    this.farewellPending = true;
    this.options.openai.send(JSON.stringify(buildFarewellResponse()));
  }

  /**
   * Called when a response completes while a hangup is in progress: either the goodbye
   * has now been generated, or the finished turn said nothing and one must be requested.
   */
  private advanceFarewell(spokeInResponse: boolean): void {
    if (this.hangup.current !== 'farewell') {
      return;
    }
    if (this.farewellPending) {
      this.farewellPending = false;
      this.hangup.beginDraining();
      return;
    }
    if (spokeInResponse) {
      this.hangup.beginDraining();
      return;
    }
    this.requestFarewellTurn();
  }
}

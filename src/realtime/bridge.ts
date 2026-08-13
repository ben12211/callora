import type { AgentConfig } from '../domain/models.js';
import {
  END_CALL_TOOL_NAME,
  buildAudioAppend,
  buildEndCallToolOutput,
  buildFarewellResponse,
  buildGreetingResponse,
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

/** Silence, measured from the last caller speech, before the agent checks in once. */
export const DEFAULT_SILENCE_PROMPT_MS = 12_000;
/** Further silence after that unanswered check before the agent says goodbye and hangs up. */
export const DEFAULT_SILENCE_HANGUP_MS = 12_000;
/** Ceiling on waiting for the goodbye audio to finish playing before hanging up anyway. */
export const FAREWELL_TIMEOUT_MS = 15_000;
/** Ceiling on waiting for Twilio marks after the goodbye response completed. */
export const FAREWELL_DRAIN_TIMEOUT_MS = 5_000;

export interface SilenceOptions {
  promptAfterMs?: number;
  hangupAfterMs?: number;
}

/** Progress of a hangup, from the model's request to the terminated Twilio call. */
type EndState = 'none' | 'farewell' | 'draining' | 'terminating' | 'terminated';

/** Minimal duplex text channel; implemented by both WebSocket ends and by tests. */
export interface MessageChannel {
  send(payload: string): void;
  close(): void;
  onMessage(handler: (raw: string) => void): void;
  onClose(handler: () => void): void;
  onError(handler: (error: Error) => void): void;
}

export interface BridgeLogger {
  info(details: Record<string, unknown>, message: string): void;
  warn(details: Record<string, unknown>, message: string): void;
  error(details: Record<string, unknown>, message: string): void;
}

export interface BridgeOptions {
  twilio: MessageChannel;
  openai: MessageChannel;
  agent: AgentConfig;
  businessId: string;
  callSid: string;
  callerNumber?: string | null;
  logger: BridgeLogger;
  /** Called once the Twilio stream and, when known, the OpenAI session are identified. */
  onIdentifiers?: (identifiers: { streamSid: string | null; openaiSessionId: string | null }) => void;
  /**
   * Terminates the underlying Twilio call. The bridge supplies no identifier: the
   * caller closes over the CallSid the stream was authorized for.
   */
  endCall?: (reason: string) => Promise<void>;
  silence?: SilenceOptions;
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

  private endState: EndState = 'none';
  private endReason: string | null = null;
  /** True between `response.created` and `response.done`; a second response.create would be rejected. */
  private responseActive = false;
  /** Whether the in-flight response has produced any audio for the caller. */
  private assistantAudioInResponse = false;
  /** True once a goodbye has been asked for and is still being generated. */
  private farewellPending = false;
  /** `call_id`s of `end_call` tool calls already answered, so retries stay idempotent. */
  private readonly handledEndCallIds = new Set<string>();
  private endTimer: ReturnType<typeof setTimeout> | null = null;

  /** 0 = caller last spoke normally, 1 = the "are you still there?" check was asked. */
  private silenceStage: 0 | 1 = 0;
  private silenceTimer: ReturnType<typeof setTimeout> | null = null;

  public constructor(private readonly options: BridgeOptions) {}

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

    openai.send(JSON.stringify(buildSessionUpdate({ agent, callerNumber })));
  }

  public close(reason: string): void {
    if (this.closed) {
      return;
    }
    this.closed = true;
    this.clearSilenceTimer();
    this.clearEndTimer();
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
        this.options.onIdentifiers?.({ streamSid: this.streamSid, openaiSessionId: this.openaiSessionId });
        this.restartSilenceTimer();
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
        this.options.onIdentifiers?.({ streamSid: this.streamSid, openaiSessionId: this.openaiSessionId });
        if (!this.greeted) {
          this.greeted = true;
          this.options.openai.send(JSON.stringify(buildGreetingResponse(this.options.agent)));
        }
        return;
      }
      case 'response.created': {
        this.responseActive = true;
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
        if (this.responseStartTimestamp === null) {
          this.responseStartTimestamp = this.latestMediaTimestamp;
        }
        this.lastAssistantItemId = readString(message, 'item_id') ?? this.lastAssistantItemId;
        this.options.twilio.send(JSON.stringify(buildTwilioMedia(this.streamSid, delta)));
        this.options.twilio.send(JSON.stringify(buildTwilioMark(this.streamSid)));
        this.pendingMarks += 1;
        return;
      }
      case 'input_audio_buffer.speech_started': {
        // The caller is talking, so the silence escalation starts over from scratch.
        this.silenceStage = 0;
        this.restartSilenceTimer();
        this.handleBargeIn();
        return;
      }
      case 'input_audio_buffer.speech_stopped': {
        this.restartSilenceTimer();
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

        if (this.endState === 'none') {
          this.restartSilenceTimer();
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

  /** Caller started speaking: drop queued assistant audio and rewind the model's item. */
  private handleBargeIn(): void {
    if (this.endState === 'farewell' || this.endState === 'draining') {
      // The caller talked over the goodbye. Stop the audio and hang up now rather than
      // waiting for marks that will never arrive.
      if (this.streamSid) {
        this.options.twilio.send(JSON.stringify(buildTwilioClear(this.streamSid)));
      }
      this.pendingMarks = 0;
      this.terminate('interrupted-farewell');
      return;
    }
    if (this.pendingMarks === 0 || this.responseStartTimestamp === null || !this.streamSid) {
      return;
    }

    if (this.lastAssistantItemId) {
      const elapsedMs = this.latestMediaTimestamp - this.responseStartTimestamp;
      this.options.openai.send(JSON.stringify(buildTruncate(this.lastAssistantItemId, elapsedMs)));
    }
    this.options.twilio.send(JSON.stringify(buildTwilioClear(this.streamSid)));

    this.pendingMarks = 0;
    this.lastAssistantItemId = null;
    this.responseStartTimestamp = null;
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

    if (this.endState !== 'none') {
      if (callId) {
        this.options.openai.send(JSON.stringify(buildEndCallToolOutput(callId, true)));
      }
      this.options.logger.info(
        {
          businessId: this.options.businessId,
          callSid: this.options.callSid,
          reason,
          state: this.endState,
        },
        'Ignoring a duplicate end_call request',
      );
      return;
    }

    this.endState = 'farewell';
    this.endReason = reason;
    this.clearSilenceTimer();
    this.options.logger.info(
      { businessId: this.options.businessId, callSid: this.options.callSid, reason },
      'Ending the call after a short goodbye',
    );

    if (callId) {
      this.options.openai.send(JSON.stringify(buildEndCallToolOutput(callId, false)));
    }
    this.armEndTimer(FAREWELL_TIMEOUT_MS, 'farewell-timeout');

    if (alreadySaidGoodbye) {
      // The model said its goodbye in the turn that called the tool; asking for another
      // would just repeat it, so wait for that audio to finish playing.
      this.beginDraining();
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
    if (this.endState !== 'farewell') {
      return;
    }
    if (this.farewellPending) {
      this.farewellPending = false;
      this.beginDraining();
      return;
    }
    if (spokeInResponse) {
      this.beginDraining();
      return;
    }
    this.requestFarewellTurn();
  }

  /** The goodbye is fully generated; wait for Twilio to confirm it actually played. */
  private beginDraining(): void {
    this.endState = 'draining';
    this.armEndTimer(FAREWELL_DRAIN_TIMEOUT_MS, 'farewell-drain-timeout');
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
    this.clearSilenceTimer();

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

  /**
   * Server VAD reports when the caller speaks; the absence of those events is what we
   * treat as silence. First escalation asks once whether they are still there, the
   * second says goodbye and hangs up. Any caller speech resets both.
   */
  private restartSilenceTimer(): void {
    this.clearSilenceTimer();
    if (this.closed || this.endState !== 'none' || !this.streamSid) {
      return;
    }

    const { promptAfterMs = DEFAULT_SILENCE_PROMPT_MS, hangupAfterMs = DEFAULT_SILENCE_HANGUP_MS } =
      this.options.silence ?? {};
    const delay = this.silenceStage === 0 ? promptAfterMs : hangupAfterMs;

    this.silenceTimer = setTimeout(() => {
      this.silenceTimer = null;
      this.handleSilenceTimeout();
    }, delay);
    this.silenceTimer.unref?.();
  }

  private handleSilenceTimeout(): void {
    if (this.closed || this.endState !== 'none') {
      return;
    }
    // The agent is still speaking, so the caller has not actually been left in silence.
    if (this.pendingMarks > 0 || this.responseStartTimestamp !== null) {
      this.restartSilenceTimer();
      return;
    }

    if (this.silenceStage === 0) {
      this.silenceStage = 1;
      this.options.logger.info(
        { businessId: this.options.businessId, callSid: this.options.callSid },
        'Caller silent; asking whether they are still on the line',
      );
      this.options.openai.send(JSON.stringify(buildStillThereResponse()));
      this.restartSilenceTimer();
      return;
    }

    this.requestEndCall('caller_silent');
  }

  private clearSilenceTimer(): void {
    if (this.silenceTimer) {
      clearTimeout(this.silenceTimer);
      this.silenceTimer = null;
    }
  }
}

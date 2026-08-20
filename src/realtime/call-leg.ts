/**
 * Provider-neutral pieces of a bridged call.
 *
 * Every provider bridge pairs one Twilio Media Stream with one provider session, and the
 * Twilio side of that pairing is identical no matter who is generating the audio: the
 * same mark accounting, the same drain-then-hang-up sequence, the same silence
 * escalation. Keeping those here is what stops the three bridges from drifting apart —
 * silence handling existed only on the OpenAI bridge precisely because each one owned
 * its own copy.
 */

/** Minimal duplex text channel; implemented by both WebSocket ends and by tests. */
export interface MessageChannel {
  send(payload: string): void;
  close(): void;
  onMessage(handler: (raw: string) => void): void;
  onClose(handler: () => void): void;
  onError(handler: (error: Error) => void): void;
}

export interface BridgeLogger {
  debug(details: Record<string, unknown>, message: string): void;
  info(details: Record<string, unknown>, message: string): void;
  warn(details: Record<string, unknown>, message: string): void;
  error(details: Record<string, unknown>, message: string): void;
}

/** Keeps one conversation line readable in a log tail. */
export const MAX_LOGGED_TRANSCRIPT_CHARS = 500;

export function truncateTranscript(text: string, limit = MAX_LOGGED_TRANSCRIPT_CHARS): string {
  const collapsed = text.replace(/\s+/g, ' ').trim();
  return collapsed.length > limit ? `${collapsed.slice(0, limit)}…` : collapsed;
}

/** Progress of a hangup, from the model's request to the terminated Twilio call. */
export type EndState = 'none' | 'farewell' | 'draining' | 'terminating' | 'terminated';

/** Ceiling on waiting for the goodbye audio to finish playing before hanging up anyway. */
export const FAREWELL_TIMEOUT_MS = 15_000;
/** Ceiling on waiting for Twilio marks after the goodbye response completed. */
export const FAREWELL_DRAIN_TIMEOUT_MS = 5_000;

export interface HangupSequenceOptions {
  businessId: string;
  callSid: string;
  logger: BridgeLogger;
  /**
   * Terminates the underlying Twilio call. The sequence supplies no identifier: the
   * caller closes over the CallSid the stream was authorized for.
   */
  endCall?: (reason: string) => Promise<void>;
  /** True once everything the caller still has to hear has actually played. */
  audioDrained: () => boolean;
  /** Tears the bridge down; called exactly once, after the Twilio call is gone. */
  onFinished: (reason: string) => void;
  /** How long to wait for Twilio marks once the goodbye is fully generated. */
  drainTimeoutMs?: number;
}

/**
 * Drives the end of a call: the agent asks to hang up, the goodbye is allowed to reach
 * the caller, and only then is the Twilio call terminated — idempotently, with a
 * media-stream-close fallback when the REST hangup fails.
 *
 * Providers differ only in how they get *to* `beginDraining`. OpenAI has to ask the model
 * for a closing turn first (`enterFarewell`); ElevenLabs and Cartesia already have the
 * goodbye in flight when the tool call arrives.
 */
export class HangupSequence {
  private state: EndState = 'none';
  private reason: string | null = null;
  private timer: ReturnType<typeof setTimeout> | null = null;

  public constructor(private readonly options: HangupSequenceOptions) {}

  public get current(): EndState {
    return this.state;
  }

  /** True once a hangup has been requested, in any of its stages. */
  public get active(): boolean {
    return this.state !== 'none';
  }

  /** True while the caller may still be hearing the goodbye. */
  public get closing(): boolean {
    return this.state === 'farewell' || this.state === 'draining';
  }

  public get requestedReason(): string | null {
    return this.reason;
  }

  /**
   * OpenAI only: the model has asked to hang up but has not spoken its goodbye yet, so
   * the sequence waits on the response lifecycle before draining.
   */
  public enterFarewell(reason: string, timeoutMs = FAREWELL_TIMEOUT_MS): void {
    this.state = 'farewell';
    this.reason = reason;
    this.armTimer(timeoutMs, 'farewell-timeout');
  }

  /** The goodbye is fully generated; wait for Twilio to confirm it actually played. */
  public beginDraining(reason?: string): void {
    this.state = 'draining';
    if (reason) {
      this.reason = reason;
    }
    this.armTimer(this.options.drainTimeoutMs ?? FAREWELL_DRAIN_TIMEOUT_MS, 'farewell-drain-timeout');
    this.terminateWhenDrained();
  }

  /** Called whenever a drain condition may have been satisfied (a mark, a `done`). */
  public terminateWhenDrained(): void {
    if (this.state === 'draining' && this.options.audioDrained()) {
      this.terminate(this.reason ?? 'end-call');
    }
  }

  /** Idempotent: only the first call reaches Twilio, and only once. */
  public terminate(reason: string): void {
    if (this.state === 'terminating' || this.state === 'terminated') {
      return;
    }
    this.state = 'terminating';
    this.clearTimer();

    const { businessId, callSid, logger, endCall, onFinished } = this.options;
    const finish = (): void => {
      this.state = 'terminated';
      onFinished(reason);
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

  public dispose(): void {
    this.clearTimer();
  }

  private armTimer(delayMs: number, reason: string): void {
    this.clearTimer();
    this.timer = setTimeout(() => {
      this.timer = null;
      this.options.logger.warn(
        { businessId: this.options.businessId, callSid: this.options.callSid, reason },
        'Goodbye audio did not complete in time; hanging up anyway',
      );
      this.terminate(reason);
    }, delayMs);
    this.timer.unref?.();
  }

  private clearTimer(): void {
    if (this.timer) {
      clearTimeout(this.timer);
      this.timer = null;
    }
  }
}

/** Silence, measured from the last caller speech, before the agent checks in once. */
export const DEFAULT_SILENCE_PROMPT_MS = 12_000;
/** Further silence after that unanswered check before the agent says goodbye and hangs up. */
export const DEFAULT_SILENCE_HANGUP_MS = 12_000;

export interface SilenceOptions {
  promptAfterMs?: number;
  hangupAfterMs?: number;
}

export interface SilenceWatchdogOptions extends SilenceOptions {
  /** False once the bridge is closed or a hangup is already under way. */
  armed: () => boolean;
  /** True while the agent is still speaking, which is not the caller being silent. */
  agentSpeaking: () => boolean;
  /**
   * First escalation: ask once whether the caller is still there.
   *
   * Optional, because not every provider gives Callora a way to make the agent say
   * something unprompted. Without it the watchdog runs one stage and hangs up after
   * `promptAfterMs + hangupAfterMs` of silence rather than inventing a protocol message.
   */
  onPrompt?: () => void;
  /** Second escalation: say goodbye and hang up. */
  onHangup: () => void;
}

/**
 * Two-stage silence escalation, shared by every provider.
 *
 * A caller who stops responding otherwise holds a Twilio leg and a provider session open
 * until the hour-long ceiling, which is both a bad experience and a real bill.
 */
export class SilenceWatchdog {
  /** 0 = caller last spoke normally, 1 = the "are you still there?" check was asked. */
  private stage: 0 | 1 = 0;
  private timer: ReturnType<typeof setTimeout> | null = null;

  public constructor(private readonly options: SilenceWatchdogOptions) {}

  /** The caller spoke: escalation starts over from scratch. */
  public reset(): void {
    this.stage = 0;
    this.restart();
  }

  public restart(): void {
    this.stop();
    if (!this.options.armed()) {
      return;
    }
    const {
      promptAfterMs = DEFAULT_SILENCE_PROMPT_MS,
      hangupAfterMs = DEFAULT_SILENCE_HANGUP_MS,
    } = this.options;
    const delay = this.options.onPrompt
      ? (this.stage === 0 ? promptAfterMs : hangupAfterMs)
      : promptAfterMs + hangupAfterMs;

    this.timer = setTimeout(() => {
      this.timer = null;
      this.fire();
    }, delay);
    this.timer.unref?.();
  }

  public stop(): void {
    if (this.timer) {
      clearTimeout(this.timer);
      this.timer = null;
    }
  }

  private fire(): void {
    if (!this.options.armed()) {
      return;
    }
    // The agent is still speaking, so the caller has not actually been left in silence.
    if (this.options.agentSpeaking()) {
      this.restart();
      return;
    }

    if (this.stage === 0 && this.options.onPrompt) {
      this.stage = 1;
      this.options.onPrompt();
      this.restart();
      return;
    }

    this.options.onHangup();
  }
}

import { performance } from 'node:perf_hooks';

/** 20 ms of 8 kHz mu-law: the frame size Twilio itself uses on the media stream. */
export const TWILIO_FRAME_BYTES = 160;
const BYTES_PER_MS = 8;

export interface PlayoutOptions {
  /** Sends one base64 mu-law frame to Twilio. */
  sendFrame: (payload: string) => void;
  /** Sends a mark after the last queued frame, so Twilio reports when it has played. */
  sendMark: () => void;
  /** Audio held back before a reply starts, absorbing the provider's first uneven chunks. */
  prebufferMs?: number;
  /** How far ahead of real time Twilio is kept; covers event-loop jitter. */
  leadMs?: number;
  /** Ceiling on queued audio; beyond it new audio is refused and counted as an overrun. */
  maxBufferMs?: number;
  tickMs?: number;
  now?: () => number;
  setInterval?: (callback: () => void, ms: number) => unknown;
  clearInterval?: (handle: unknown) => void;
}

export interface PlayoutStats {
  framesSent: number;
  bytesSent: number;
  maxDepthMs: number;
  underruns: number;
  overruns: number;
  droppedBytes: number;
  /** Time from the first byte of a reply arriving to its first frame going to Twilio. */
  startDelayMs: number | null;
}

/**
 * Feeds Twilio even 20 ms frames at real-time pace instead of forwarding provider chunks,
 * whose sizes (16 bytes to over a second of audio) and arrival times are uneven.
 *
 * Bytes leave in exactly the order they arrived and are never transcoded, padded or
 * repeated; the only transformation is re-cutting them into 160-byte frames.
 */
export class TwilioPlayout {
  private readonly chunks: Buffer[] = [];
  private queuedBytes = 0;
  private sourceFinished = false;
  private playing = false;
  private playStart = 0;
  private sentMs = 0;
  private timer: unknown = null;
  private firstArrivalAt: number | null = null;
  private stats: PlayoutStats = TwilioPlayout.emptyStats();

  private readonly prebufferMs: number;
  private readonly leadMs: number;
  private readonly maxBufferBytes: number;
  private readonly tickMs: number;
  private readonly now: () => number;
  private readonly startTimer: (callback: () => void, ms: number) => unknown;
  private readonly stopTimer: (handle: unknown) => void;

  public constructor(private readonly options: PlayoutOptions) {
    this.prebufferMs = options.prebufferMs ?? 80;
    this.leadMs = options.leadMs ?? 100;
    this.maxBufferBytes = (options.maxBufferMs ?? 60_000) * BYTES_PER_MS;
    this.tickMs = options.tickMs ?? 10;
    this.now = options.now ?? (() => performance.now());
    this.startTimer = options.setInterval ?? ((callback, ms) => {
      const handle = setInterval(callback, ms);
      handle.unref?.();
      return handle;
    });
    this.stopTimer = options.clearInterval ?? ((handle) => clearInterval(handle as ReturnType<typeof setInterval>));
  }

  private static emptyStats(): PlayoutStats {
    return { framesSent: 0, bytesSent: 0, maxDepthMs: 0, underruns: 0, overruns: 0, droppedBytes: 0, startDelayMs: null };
  }

  /** Audio queued here and not yet sent to Twilio. */
  public get depthMs(): number {
    return this.queuedBytes / BYTES_PER_MS;
  }

  /** Nothing queued and nothing waiting to be sent. */
  public get idle(): boolean {
    return this.queuedBytes === 0 && !this.playing;
  }

  public enqueue(audio: Buffer): void {
    if (audio.length === 0) return;
    if (this.queuedBytes + audio.length > this.maxBufferBytes) {
      this.stats.overruns += 1;
      this.stats.droppedBytes += audio.length;
      return;
    }
    this.firstArrivalAt ??= this.now();
    this.sourceFinished = false;
    this.chunks.push(audio);
    this.queuedBytes += audio.length;
    this.stats.maxDepthMs = Math.max(this.stats.maxDepthMs, this.depthMs);
    this.ensureTimer();
    this.tick();
  }

  /** The provider has sent everything for now: flush the tail even below one frame. */
  public finish(): void {
    this.sourceFinished = true;
    this.ensureTimer();
    this.tick();
  }

  /** Barge-in: drop everything not yet sent. Nothing queued before this can be sent after. */
  public clear(): void {
    this.chunks.length = 0;
    this.queuedBytes = 0;
    this.sourceFinished = false;
    this.playing = false;
    this.firstArrivalAt = null;
    this.stopTimerIfRunning();
  }

  /** Stats since the last call, for one reply's log line. */
  public takeStats(): PlayoutStats {
    const stats = this.stats;
    this.stats = TwilioPlayout.emptyStats();
    return stats;
  }

  public tick(): void {
    const now = this.now();
    if (!this.playing) {
      const ready = this.depthMs >= this.prebufferMs || (this.sourceFinished && this.queuedBytes > 0);
      if (!ready) return;
      this.playing = true;
      this.playStart = now;
      this.sentMs = 0;
      if (this.firstArrivalAt !== null && this.stats.startDelayMs === null) {
        this.stats.startDelayMs = Math.round(now - this.firstArrivalAt);
      }
    }

    while (this.sentMs - (now - this.playStart) < this.leadMs) {
      const frame = this.takeFrame();
      if (!frame) break;
      this.options.sendFrame(frame.toString('base64'));
      this.sentMs += frame.length / BYTES_PER_MS;
      this.stats.framesSent += 1;
      this.stats.bytesSent += frame.length;
    }

    if (this.queuedBytes === 0 && this.sourceFinished) {
      // Everything this reply had is with Twilio; the mark says when it has been heard.
      this.options.sendMark();
      this.playing = false;
      this.sourceFinished = false;
      this.firstArrivalAt = null;
      this.stopTimerIfRunning();
      return;
    }
    // Less than a frame left and more still coming: the provider is behind.
    if (this.queuedBytes < TWILIO_FRAME_BYTES && !this.sourceFinished) this.checkStarved(now);
  }

  /**
   * The provider fell behind: once Twilio has played everything sent, rebuffer rather than
   * trickling out fragments, which is what a stutter is.
   */
  private checkStarved(now: number): void {
    if (this.playing && now - this.playStart >= this.sentMs) {
      this.stats.underruns += 1;
      this.playing = false;
    }
  }

  private takeFrame(): Buffer | null {
    const wanted = this.queuedBytes >= TWILIO_FRAME_BYTES ? TWILIO_FRAME_BYTES : this.sourceFinished ? this.queuedBytes : 0;
    if (wanted === 0) return null;
    const frame = Buffer.allocUnsafe(wanted);
    let filled = 0;
    while (filled < wanted) {
      const head = this.chunks[0]!;
      const take = Math.min(head.length, wanted - filled);
      head.copy(frame, filled, 0, take);
      filled += take;
      if (take === head.length) this.chunks.shift();
      else this.chunks[0] = head.subarray(take);
    }
    this.queuedBytes -= wanted;
    return frame;
  }

  private ensureTimer(): void {
    if (this.timer === null) this.timer = this.startTimer(() => this.tick(), this.tickMs);
  }

  private stopTimerIfRunning(): void {
    if (this.timer !== null) {
      this.stopTimer(this.timer);
      this.timer = null;
    }
  }
}

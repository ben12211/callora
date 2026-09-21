import { performance } from 'node:perf_hooks';

/**
 * Timing and size of the agent audio one reply sends to Twilio. Logged once per reply, so a
 * call that sounds choppy can be told apart by numbers: audio arriving slower than real time
 * starves the phone; audio that arrives fine but was cut off points at barge-in instead.
 */
export class AudioTransportStats {
  private frames = 0;
  private bytes = 0;
  private minBytes = Number.POSITIVE_INFINITY;
  private maxBytes = 0;
  private firstAt: number | null = null;
  private lastAt: number | null = null;
  private gapSum = 0;
  private maxGap = 0;
  // Real-time playback model: how far the phone could have played had it started at the
  // first byte. Starvation is time the listener would have heard silence waiting for audio.
  private starvedMs = 0;
  private underruns = 0;

  public record(byteCount: number, at = performance.now()): void {
    if (this.firstAt === null) this.firstAt = at;
    if (this.lastAt !== null) {
      const gap = at - this.lastAt;
      this.gapSum += gap;
      this.maxGap = Math.max(this.maxGap, gap);
      // Audio received minus audio already played; playback pauses while starved.
      const played = this.lastAt - this.firstAt - this.starvedMs;
      const playable = this.bytes / 8 - played;
      if (gap > playable) {
        this.underruns += 1;
        this.starvedMs += gap - Math.max(0, playable);
      }
    }
    this.lastAt = at;
    this.frames += 1;
    this.bytes += byteCount;
    this.minBytes = Math.min(this.minBytes, byteCount);
    this.maxBytes = Math.max(this.maxBytes, byteCount);
  }

  public get audioMs(): number {
    return this.bytes / 8;
  }

  public summary(): Record<string, number> {
    const spanMs = this.firstAt !== null && this.lastAt !== null ? this.lastAt - this.firstAt : 0;
    return {
      frames: this.frames,
      bytes: this.bytes,
      audioMs: Math.round(this.audioMs),
      arrivalSpanMs: Math.round(spanMs),
      minFrameBytes: this.frames ? this.minBytes : 0,
      maxFrameBytes: this.maxBytes,
      avgFrameBytes: this.frames ? Math.round(this.bytes / this.frames) : 0,
      avgGapMs: this.frames > 1 ? Math.round((this.gapSum / (this.frames - 1)) * 10) / 10 : 0,
      maxGapMs: Math.round(this.maxGap),
      underruns: this.underruns,
      starvedMs: Math.round(this.starvedMs),
    };
  }
}

/** Coarse distribution of caller audio levels, for telling line noise from speech. */
export class LevelHistogram {
  // Upper bounds on the 16-bit linear RMS scale; the last bucket is open-ended.
  private static readonly BOUNDS = [50, 150, 300, 600, 900, 1500, 3000, 6000];
  private readonly counts = new Array<number>(LevelHistogram.BOUNDS.length + 1).fill(0);

  public record(rms: number): void {
    const index = LevelHistogram.BOUNDS.findIndex((bound) => rms < bound);
    const slot = index === -1 ? LevelHistogram.BOUNDS.length : index;
    this.counts[slot] = (this.counts[slot] ?? 0) + 1;
  }

  public summary(): Record<string, number> {
    const result: Record<string, number> = {};
    LevelHistogram.BOUNDS.forEach((bound, index) => { result[`lt${bound}`] = this.counts[index] ?? 0; });
    result[`ge${LevelHistogram.BOUNDS.at(-1)}`] = this.counts.at(-1) ?? 0;
    return result;
  }
}

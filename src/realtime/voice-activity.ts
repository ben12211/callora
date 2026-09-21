/**
 * Energy-based caller speech detection on Twilio's inbound mu-law audio.
 *
 * Barge-in cannot wait for the transcript: the STT on the composed pipelines sends a single
 * final once the caller stops talking, so an interruption keyed on text is only noticed
 * after the caller has already spoken over the whole reply. The inbound track carries only
 * the caller's microphone, so sustained energy on it while the agent talks is the caller.
 */

const MULAW_TO_LINEAR = new Int16Array(256);
for (let byte = 0; byte < 256; byte += 1) {
  const value = ~byte & 0xff;
  const sign = value & 0x80;
  const exponent = (value >> 4) & 0x07;
  const mantissa = value & 0x0f;
  const magnitude = (((mantissa << 3) + 0x84) << exponent) - 0x84;
  MULAW_TO_LINEAR[byte] = sign ? -magnitude : magnitude;
}

/** Root-mean-square level of a mu-law buffer, on the 16-bit linear scale. */
export function mulawRms(audio: Uint8Array): number {
  if (audio.length === 0) return 0;
  let sum = 0;
  for (const byte of audio) {
    const sample = MULAW_TO_LINEAR[byte] ?? 0;
    sum += sample * sample;
  }
  return Math.sqrt(sum / audio.length);
}

export interface VoiceActivityOptions {
  /** Level above which a frame counts as speech; ~-31 dBFS, well above line noise. */
  thresholdRms?: number;
  /** Speech that must accumulate before it counts as the caller talking. */
  triggerMs?: number;
}

export class VoiceActivityDetector {
  private readonly thresholdRms: number;
  private readonly triggerMs: number;
  private speechMs = 0;

  public constructor(options: VoiceActivityOptions = {}) {
    this.thresholdRms = options.thresholdRms ?? 900;
    this.triggerMs = options.triggerMs ?? 240;
  }

  /**
   * Feeds one inbound frame; true once the caller has been talking long enough.
   * Quiet frames drain the count twice as fast as speech fills it, so a click or a
   * breath never adds up to a barge-in, while the short gaps inside a word do not reset it.
   */
  public push(audio: Uint8Array): boolean {
    const frameMs = audio.length / 8;
    if (mulawRms(audio) >= this.thresholdRms) {
      // Capped, so the detector lets go within a fraction of a second after a long
      // monologue instead of draining it for as long as the caller talked.
      this.speechMs = Math.min(this.speechMs + frameMs, 2 * this.triggerMs);
    } else {
      this.speechMs = Math.max(0, this.speechMs - 2 * frameMs);
    }
    return this.speechMs >= this.triggerMs;
  }

  public reset(): void {
    this.speechMs = 0;
  }
}

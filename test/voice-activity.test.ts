import { describe, expect, it } from 'vitest';
import { mulawRms, VoiceActivityDetector } from '../src/realtime/voice-activity.js';

describe('mu-law voice activity', () => {
  it('decodes mu-law silence as zero and a loud code as loud', () => {
    expect(mulawRms(Buffer.alloc(160, 0xff))).toBe(0);
    expect(mulawRms(Buffer.alloc(160, 0x7f))).toBe(0);
    expect(mulawRms(Buffer.alloc(160, 0x10))).toBeGreaterThan(5_000);
  });

  it('needs sustained speech, and lets quiet frames drain it', () => {
    const detector = new VoiceActivityDetector({ thresholdRms: 900, triggerMs: 100 });
    const loud = Buffer.alloc(160, 0x10);
    const quiet = Buffer.alloc(160, 0xff);
    expect([1, 2, 3, 4].map(() => detector.push(loud))).toEqual([false, false, false, false]);
    expect(detector.push(quiet)).toBe(false);
    expect(detector.push(quiet)).toBe(false);
    expect([1, 2, 3, 4, 5].map(() => detector.push(loud)).at(-1)).toBe(true);
    detector.reset();
    expect(detector.push(loud)).toBe(false);
  });
});

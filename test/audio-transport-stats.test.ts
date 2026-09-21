import { describe, expect, it } from 'vitest';
import { AudioTransportStats, LevelHistogram } from '../src/realtime/audio-transport-stats.js';

describe('agent audio delivery stats', () => {
  it('reports no underrun when audio arrives faster than real time', () => {
    const stats = new AudioTransportStats();
    // 160 bytes (20 ms of audio) every 5 ms.
    for (let index = 0; index < 50; index += 1) stats.record(160, index * 5);
    expect(stats.summary()).toEqual(expect.objectContaining({ frames: 50, audioMs: 1000, underruns: 0, starvedMs: 0, maxGapMs: 5 }));
  });

  it('counts the silence a listener would hear while audio arrives too slowly', () => {
    const stats = new AudioTransportStats();
    stats.record(160, 0); // 20 ms of audio
    stats.record(160, 100); // arrived 80 ms after the first ran out
    stats.record(160, 110);
    expect(stats.summary()).toEqual(expect.objectContaining({ underruns: 1, starvedMs: 80, maxGapMs: 100 }));
  });
});

describe('caller level histogram', () => {
  it('buckets levels by upper bound', () => {
    const histogram = new LevelHistogram();
    [0, 49, 50, 899, 900, 10_000].forEach((level) => histogram.record(level));
    expect(histogram.summary()).toEqual(expect.objectContaining({ lt50: 2, lt150: 1, lt900: 1, lt1500: 1, ge6000: 1 }));
  });
});

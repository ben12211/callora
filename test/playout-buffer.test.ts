import { describe, expect, it } from 'vitest';
import { TwilioPlayout } from '../src/realtime/playout-buffer.js';

function harness(options: { prebufferMs?: number; leadMs?: number; maxBufferMs?: number } = {}) {
  let clock = 0;
  let tick: (() => void) | null = null;
  const frames: { at: number; bytes: Buffer }[] = [];
  let marks = 0;
  const playout = new TwilioPlayout({
    ...options,
    sendFrame: (payload) => frames.push({ at: clock, bytes: Buffer.from(payload, 'base64') }),
    sendMark: () => { marks += 1; },
    now: () => clock,
    setInterval: (callback) => { tick = callback; return 1; },
    clearInterval: () => { tick = null; },
  });
  const advance = (ms: number): void => {
    for (let step = 0; step < ms; step += 10) {
      clock += 10;
      tick?.();
    }
  };
  return { playout, frames, advance, marks: () => marks, timerRunning: () => tick !== null };
}

const bytes = (length: number, seed = 0): Buffer => Buffer.from(Array.from({ length }, (_, index) => (index + seed) % 256));

describe('Twilio playout buffer', () => {
  it('sends every byte once, in order, in 160-byte frames', () => {
    const { playout, frames, advance, marks } = harness();
    const audio = bytes(8_000 + 37);
    // Provider chunk sizes as observed from Deepdub: tiny, huge and odd.
    let offset = 0;
    for (const size of [16, 1_707, 110, 3_000, 17, 3_187]) {
      playout.enqueue(audio.subarray(offset, offset + size));
      offset += size;
    }
    playout.finish();
    advance(2_000);

    expect(Buffer.concat(frames.map((frame) => frame.bytes))).toEqual(audio);
    expect(frames.slice(0, -1).every((frame) => frame.bytes.length === 160)).toBe(true);
    expect(marks()).toBe(1);
  });

  it('paces frames at real time, a fixed lead ahead, however fast the provider is', () => {
    const { playout, frames, advance } = harness({ leadMs: 100 });
    playout.enqueue(bytes(8_000)); // one second of audio, all at once
    playout.finish();
    // Immediately: only the lead (100 ms = 5 frames) has gone out.
    expect(frames).toHaveLength(5);
    advance(500);
    // After 500 ms, Twilio holds 500 ms played + 100 ms lead = 30 frames.
    expect(frames).toHaveLength(30);
    advance(600);
    expect(frames).toHaveLength(50);
  });

  it('holds a reply back until the prebuffer is full, unless the reply is shorter', () => {
    const { playout, frames } = harness({ prebufferMs: 80 });
    playout.enqueue(bytes(320)); // 40 ms
    expect(frames).toHaveLength(0);
    playout.enqueue(bytes(320)); // 80 ms
    expect(frames.length).toBeGreaterThan(0);

    const short = harness({ prebufferMs: 80 });
    short.playout.enqueue(bytes(100));
    short.playout.finish();
    expect(short.frames).toHaveLength(1);
  });

  it('counts an underrun and rebuffers instead of trickling fragments', () => {
    const { playout, frames, advance } = harness({ prebufferMs: 80, leadMs: 100 });
    playout.enqueue(bytes(800)); // 100 ms
    advance(300); // provider stalls well past what Twilio holds
    const sentBeforeStall = frames.length;
    playout.enqueue(bytes(320)); // 40 ms arrives: below the prebuffer, so it waits
    advance(20);
    expect(frames).toHaveLength(sentBeforeStall);
    playout.enqueue(bytes(320));
    advance(10);
    expect(frames.length).toBeGreaterThan(sentBeforeStall);
    expect(playout.takeStats().underruns).toBe(1);
  });

  it('refuses audio past its bound and counts the overrun', () => {
    const { playout } = harness({ maxBufferMs: 100, prebufferMs: 1_000 });
    playout.enqueue(bytes(800)); // 100 ms: at the bound
    playout.enqueue(bytes(8)); // over it
    expect(playout.takeStats()).toEqual(expect.objectContaining({ overruns: 1, droppedBytes: 8, maxDepthMs: 100 }));
  });

  it('drops queued audio on clear, so none of it reaches Twilio afterwards', () => {
    const { playout, frames, advance, marks, timerRunning } = harness();
    playout.enqueue(bytes(8_000, 1));
    const sent = frames.length;
    playout.clear();
    expect(timerRunning()).toBe(false);
    advance(2_000);
    expect(frames).toHaveLength(sent);
    expect(marks()).toBe(0);

    playout.enqueue(bytes(400, 7));
    playout.finish();
    advance(200);
    expect(Buffer.concat(frames.slice(sent).map((frame) => frame.bytes))).toEqual(bytes(400, 7));
  });
});

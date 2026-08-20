import { describe, expect, it } from 'vitest';
import { MediaStreamBridge } from '../src/realtime/bridge.js';
import { MemoryStore, firstBusinessId } from './support/memory-store.js';
import { FakeChannel, agent, callSid, silentLogger, streamSid } from './support/realtime-harness.js';

/**
 * Transcripts were written to stdout and nowhere else: no call review, no QA, no
 * analytics, while the caller's own words still ended up in a log aggregator with no
 * retention policy attached.
 */

describe('a bridge reports its turns for persistence', () => {
  function startBridge() {
    const twilio = new FakeChannel();
    const openai = new FakeChannel();
    const turns: { speaker: 'caller' | 'agent'; content: string }[] = [];
    const bridge = new MediaStreamBridge({
      twilio,
      openai,
      agent,
      businessId: firstBusinessId,
      callSid,
      logger: silentLogger,
      onTranscript: (turn) => turns.push(turn),
    });
    bridge.start();
    twilio.emit({ event: 'start', streamSid, start: { streamSid, callSid, accountSid: 'AC1' } });
    openai.emit({ type: 'session.created', session: { id: 'sess_1' } });
    return { openai, turns };
  }

  it('labels the caller and the agent', () => {
    const { openai, turns } = startBridge();

    openai.emit({
      type: 'conversation.item.input_audio_transcription.completed',
      transcript: 'I need to change my appointment',
    });
    openai.emit({
      type: 'response.output_audio_transcript.done',
      transcript: 'Sure — which day works?',
    });

    expect(turns).toEqual([
      { speaker: 'caller', content: 'I need to change my appointment' },
      { speaker: 'agent', content: 'Sure — which day works?' },
    ]);
  });

  it('reports nothing for an empty transcript', () => {
    const { openai, turns } = startBridge();
    openai.emit({ type: 'conversation.item.input_audio_transcription.completed', transcript: '   ' });
    expect(turns).toEqual([]);
  });
});

describe('transcript storage', () => {
  const callId = 'call-1';

  it('keeps turns in order and numbers them per call', async () => {
    const store = new MemoryStore();
    await store.appendTranscriptTurn({
      callId,
      businessId: firstBusinessId,
      speaker: 'caller',
      content: 'first',
      turn: 1,
    });
    await store.appendTranscriptTurn({
      callId,
      businessId: firstBusinessId,
      speaker: 'agent',
      content: 'second',
      turn: 2,
    });

    const stored = await store.listTranscript(callId);
    expect(stored.map((turn) => turn.content)).toEqual(['first', 'second']);
    expect(stored.map((turn) => turn.speaker)).toEqual(['caller', 'agent']);
  });

  it('treats a repeated turn number as a correction, not a duplicate', async () => {
    const store = new MemoryStore();
    await store.appendTranscriptTurn({
      callId,
      businessId: firstBusinessId,
      speaker: 'caller',
      content: 'partial',
      turn: 1,
    });
    await store.appendTranscriptTurn({
      callId,
      businessId: firstBusinessId,
      speaker: 'caller',
      content: 'partial, corrected',
      turn: 1,
    });

    const stored = await store.listTranscript(callId);
    expect(stored).toHaveLength(1);
    expect(stored[0]?.content).toBe('partial, corrected');
  });

  it('prunes turns past the retention window and keeps the rest', async () => {
    const store = new MemoryStore();
    await store.appendTranscriptTurn({
      callId,
      businessId: firstBusinessId,
      speaker: 'caller',
      content: 'old',
      turn: 1,
    });
    const old = store.transcripts[0];
    if (old) {
      old.createdAt = new Date(Date.now() - 60 * 24 * 60 * 60 * 1000);
    }
    await store.appendTranscriptTurn({
      callId,
      businessId: firstBusinessId,
      speaker: 'agent',
      content: 'recent',
      turn: 2,
    });

    const cutoff = new Date(Date.now() - 30 * 24 * 60 * 60 * 1000);
    await expect(store.deleteTranscriptsOlderThan(cutoff)).resolves.toBe(1);
    expect((await store.listTranscript(callId)).map((turn) => turn.content)).toEqual(['recent']);
  });

  it('keeps one call transcript out of another', async () => {
    const store = new MemoryStore();
    await store.appendTranscriptTurn({
      callId: 'call-a',
      businessId: firstBusinessId,
      speaker: 'caller',
      content: 'for a',
      turn: 1,
    });
    await store.appendTranscriptTurn({
      callId: 'call-b',
      businessId: firstBusinessId,
      speaker: 'caller',
      content: 'for b',
      turn: 1,
    });

    expect((await store.listTranscript('call-a')).map((turn) => turn.content)).toEqual(['for a']);
    expect((await store.listTranscript('call-b')).map((turn) => turn.content)).toEqual(['for b']);
  });
});

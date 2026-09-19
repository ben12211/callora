import { readFile } from 'node:fs/promises';
import { EventEmitter } from 'node:events';
import { describe, expect, it, vi } from 'vitest';
import WebSocket from 'ws';
import { loadConfig, missingProviderCredentials } from '../src/config.js';
import type { PronunciationEntry } from '../src/domain/models.js';
import { HebrewStreamingChunker } from '../src/hebrew/chunker.js';
import { normalizePronunciationKey, normalizeSpokenHebrew } from '../src/hebrew/normalizer.js';
import { ReNikudClient } from '../src/hebrew/renikud-client.js';
import { detectHebrewPronunciationRisk } from '../src/hebrew/risk-detector.js';
import { HebrewSpeechFrontend, PronunciationCache } from '../src/hebrew/speech-frontend.js';
import { DeepdubTtsSession, renderDeepdubText } from '../src/realtime/deepdub-connection.js';
import type { SpeechPreparation } from '../src/hebrew/types.js';
import { MetricsRegistry } from '../src/platform/metrics.js';
import { MemoryStore, firstBusinessId, secondBusinessId } from './support/memory-store.js';

const baseEnv = {
  NODE_ENV: 'test', DATABASE_URL: 'postgresql://unused',
  TWILIO_ACCOUNT_SID: 'AC00000000000000000000000000000000',
  TWILIO_AUTH_TOKEN: 'test-auth-token', PUBLIC_BASE_URL: 'https://voice.example.test',
} satisfies NodeJS.ProcessEnv;

function entry(businessId: string, sourceText = "ז'בוטינסקי"): PronunciationEntry {
  const now = new Date();
  return { id: `${businessId}-entry`, businessId, sourceText, normalizedText: normalizePronunciationKey(sourceText), pronunciation: 'ʒabotˈinski', pronunciationType: 'ipa', locale: 'he-IL', createdAt: now, updatedAt: now };
}

function renikud(fetchImpl: typeof fetch, timeoutMs = 50): ReNikudClient {
  return new ReNikudClient({ baseUrl: 'http://renikud.test', timeoutMs, fetchImpl });
}

const successFetch = (async () => new Response(JSON.stringify({ pronunciation: 'ʃalˈom', processingMs: 12 }), { status: 200, headers: { 'content-type': 'application/json' } })) as typeof fetch;

describe('Deepdub configuration', () => {
  it('builds the documented low-latency Hebrew telephony configuration', () => {
    const config = loadConfig({ ...baseEnv, VOICE_PROVIDER: 'deepdub', DEEPDUB_API_KEY: 'dd-test', DEEPDUB_VOICE_ID: 'voice-id', CARTESIA_API_KEY: 'car-test', OPENAI_API_KEY: 'sk-test' });
    const deepdub = config.providers.deepdub!;
    expect(config.voiceProvider).toBe('deepdub');
    expect(deepdub.model).toBe('dd-etts-3.0');
    expect(deepdub.locale).toBe('he-IL');
    expect(deepdub.wsUrl).toBe('wss://wss.deepdub.ai/ws');
    expect(deepdub.firstAudioTimeoutMs).toBe(50);
    expect(deepdub.enableLogging).toBe(false);
  });

  it('requires the TTS, STT, voice, and LLM credentials without requiring ReNikud', () => {
    const env = { ...baseEnv, VOICE_PROVIDER: 'deepdub', DEEPDUB_API_KEY: 'dd', DEEPDUB_VOICE_ID: 'voice', CARTESIA_API_KEY: 'car', OPENAI_API_KEY: 'sk' };
    expect(missingProviderCredentials(env)).toEqual([]);
    expect(missingProviderCredentials({ ...env, CARTESIA_API_KEY: '' })).toEqual(['CARTESIA_API_KEY']);
    expect(loadConfig(env).providers.deepdub?.renikudUrl).toBeUndefined();
  });
});

describe('Hebrew streaming chunker', () => {
  it('starts at natural Hebrew clause boundaries before the whole reply exists', () => {
    const chunker = new HebrewStreamingChunker();
    expect(chunker.push('בשמחה, ')).toEqual(['בשמחה,']);
    expect(chunker.push('אני בודק לך עכשיו. ')).toEqual([' אני בודק לך עכשיו.']);
  });

  it('does not split times, dates, decimals, or mixed brand tokens', () => {
    const chunker = new HebrewStreamingChunker();
    expect(chunker.push('התור ב-14:30 בתאריך 12/09/2026, ')).toEqual(['התור ב-14:30 בתאריך 12/09/2026,']);
    expect(chunker.push('והמחיר 19.90 ב-PayBox. ')).toEqual([' והמחיר 19.90 ב-PayBox.']);
  });

  it('caps an unpunctuated chunk without cutting the nearest word', () => {
    const chunker = new HebrewStreamingChunker(8, 30);
    const chunks = chunker.push('זה משפט ארוך מאוד בלי סימן פיסוק ולכן צריך לחלק אותו בבטחה');
    expect(chunks[0]?.length).toBeLessThanOrEqual(31);
    expect(chunks[0]?.endsWith(' ')).toBe(true);
  });
});

describe('Hebrew risk and normalization', () => {
  it('keeps ordinary Hebrew on the fast path', () => expect(detectHebrewPronunciationRisk('שלום, איך אפשר לעזור?')).toEqual({ risky: false, reasons: [] }));
  it('detects phone, date, acronym, geresh, and mixed-script risks', () => {
    const result = detectHebrewPronunciationRisk("שלח WhatsApp למד״א ב-12/09/2026, טלפון 050-123-4567");
    expect(result.reasons).toEqual(expect.arrayContaining(['number', 'date', 'phone-or-id', 'geresh', 'mixed-script']));
  });
  it('adds deterministic spoken context to values', () => {
    const value = normalizeSpokenHebrew('המחיר ₪249, הנחה 10%, טלפון 050-123-4567 בשעה 14:30');
    expect(value).toContain('249 שקלים');
    expect(value).toContain('10 אחוזים');
    expect(value).toContain('מספר טלפון 0 5 0 1 2 3 4 5 6 7');
    expect(value).toContain('בשעה 14 ו-30 דקות');
  });
});

describe('Hebrew speech frontend', () => {
  it('OFF returns the original text and never calls ReNikud', async () => {
    const fetchImpl = vi.fn(successFetch) as unknown as typeof fetch;
    const frontend = new HebrewSpeechFrontend({ businessId: firstBusinessId, locale: 'he-IL', mode: 'off', dictionary: [], renikud: renikud(fetchImpl), cache: new PronunciationCache() });
    expect(await frontend.preprocess('שלום 123')).toEqual(expect.objectContaining({ spokenText: 'שלום 123', source: 'off' }));
    expect(fetchImpl).not.toHaveBeenCalled();
  });

  it('SMART avoids ReNikud for easy Hebrew and invokes it for risky Hebrew', async () => {
    const fetchImpl = vi.fn(successFetch) as unknown as typeof fetch;
    const frontend = new HebrewSpeechFrontend({ businessId: firstBusinessId, locale: 'he-IL', mode: 'smart', dictionary: [], renikud: renikud(fetchImpl), cache: new PronunciationCache() });
    expect((await frontend.preprocess('שלום לכולם')).source).toBe('native');
    expect(fetchImpl).not.toHaveBeenCalled();
    expect((await frontend.preprocess('שלח ל-PayBox')).source).toBe('renikud');
    expect(fetchImpl).toHaveBeenCalledOnce();
  });

  it('STRICT invokes assistance even for ordinary Hebrew', async () => {
    const fetchImpl = vi.fn(successFetch) as unknown as typeof fetch;
    const frontend = new HebrewSpeechFrontend({ businessId: firstBusinessId, locale: 'he-IL', mode: 'strict', dictionary: [], renikud: renikud(fetchImpl), cache: new PronunciationCache() });
    expect((await frontend.preprocess('שלום לכולם')).source).toBe('renikud');
  });

  it('uses the business dictionary before ReNikud and caches the result', async () => {
    const fetchImpl = vi.fn(successFetch) as unknown as typeof fetch;
    const frontend = new HebrewSpeechFrontend({ businessId: firstBusinessId, locale: 'he-IL', mode: 'smart', dictionary: [entry(firstBusinessId)], renikud: renikud(fetchImpl), cache: new PronunciationCache() });
    const first = await frontend.preprocess("הסניף בז'בוטינסקי");
    const second = await frontend.preprocess("הסניף בז'בוטינסקי");
    expect(first.source).toBe('dictionary');
    expect(second).toEqual(expect.objectContaining({ source: 'cache', cacheHit: true }));
    expect(fetchImpl).not.toHaveBeenCalled();
  });

  it('never reuses a tenant dictionary result across businesses', async () => {
    const cache = new PronunciationCache();
    const first = new HebrewSpeechFrontend({ businessId: firstBusinessId, locale: 'he-IL', mode: 'smart', dictionary: [entry(firstBusinessId)], cache });
    const second = new HebrewSpeechFrontend({ businessId: secondBusinessId, locale: 'he-IL', mode: 'smart', dictionary: [], cache });
    expect((await first.preprocess("ז'בוטינסקי")).spans).toHaveLength(1);
    expect((await second.preprocess("ז'בוטינסקי")).spans).toHaveLength(0);
  });

  it.each([
    ['unavailable', (async () => new Response('no', { status: 503 })) as typeof fetch],
    ['malformed', (async () => new Response(JSON.stringify({ pronunciation: 42 }), { status: 200 })) as typeof fetch],
  ])('falls back to native text when ReNikud is %s', async (_label, fetchImpl) => {
    const frontend = new HebrewSpeechFrontend({ businessId: firstBusinessId, locale: 'he-IL', mode: 'strict', dictionary: [], renikud: renikud(fetchImpl), cache: new PronunciationCache() });
    expect(await frontend.preprocess('שלום')).toEqual(expect.objectContaining({ spokenText: 'שלום', source: 'fallback', spans: [] }));
  });

  it('times ReNikud out without failing the call', async () => {
    const fetchImpl = ((_: string | URL | Request, init?: RequestInit) => new Promise<Response>((_resolve, reject) => init?.signal?.addEventListener('abort', () => reject(new Error('aborted')), { once: true }))) as typeof fetch;
    const frontend = new HebrewSpeechFrontend({ businessId: firstBusinessId, locale: 'he-IL', mode: 'strict', dictionary: [], renikud: renikud(fetchImpl, 10), cache: new PronunciationCache() });
    await expect(frontend.preprocess('שלום')).resolves.toEqual(expect.objectContaining({ source: 'fallback' }));
  });
});

describe('Deepdub pronunciation rendering', () => {
  it('renders validated IPA only inside the provider adapter', () => {
    const rendered = renderDeepdubText({ originalText: "ז'בוטינסקי", spokenText: "ז'בוטינסקי", locale: 'he-IL', mode: 'smart', spans: [{ sourceText: "ז'בוטינסקי", pronunciation: 'ʒabotˈinski', type: 'ipa' }], source: 'dictionary', riskReasons: [], processingMs: 1, cacheHit: false });
    expect(rendered).toContain('<phoneme alphabet="ipa" ph="ʒabotˈinski">');
  });

  it('escapes text and rejects unsafe pronunciation markup', () => {
    const rendered = renderDeepdubText({ originalText: '<config/>', spokenText: '<config/>', locale: 'he-IL', mode: 'smart', spans: [{ sourceText: '<config/>', pronunciation: '"><script>', type: 'ipa' }], source: 'dictionary', riskReasons: [], processingMs: 1, cacheHit: false });
    expect(rendered).toBe('&lt;config/&gt;');
    expect(rendered).not.toContain('<script>');
  });
});

class FakeDeepdubSocket extends EventEmitter {
  public readyState = WebSocket.OPEN;
  public sent: Record<string, unknown>[] = [];
  public send(payload: string): void { this.sent.push(JSON.parse(payload) as Record<string, unknown>); }
  public close(): void { this.readyState = WebSocket.CLOSED; this.emit('close'); }
  public emitMessage(message: Record<string, unknown>): void { this.emit('message', Buffer.from(JSON.stringify(message)), false); }
}

function preparation(text: string): SpeechPreparation {
  return { originalText: text, spokenText: text, locale: 'he-IL', mode: 'smart', spans: [], source: 'native', riskReasons: [], processingMs: 0, cacheHit: false };
}

describe('Deepdub persistent streaming session', () => {
  it('streams text and closes the turn without buffering the complete assistant reply', () => {
    const socket = new FakeDeepdubSocket();
    const session = new DeepdubTtsSession(socket as unknown as WebSocket, 'connection-1');
    session.send('turn-1', preparation('בשמחה,'), true);
    session.send('turn-1', preparation(' אני בודק.'), false);
    expect(socket.sent.map((message) => message['action'])).toEqual(['stream-text', 'stream-text', 'end-stream']);
    expect(session.sessionId()).toBe('connection-1');
  });

  it('cancels barge-in immediately and holds new speech until cancellation is acknowledged', () => {
    const socket = new FakeDeepdubSocket();
    const session = new DeepdubTtsSession(socket as unknown as WebSocket);
    session.send('old', preparation('ישן'), true);
    session.cancel('old');
    session.send('new', preparation('חדש'), false);
    expect(socket.sent.map((message) => message['action'])).toEqual(['stream-text', 'cancel']);
    socket.emitMessage({ isFinished: true, isCancelled: true });
    expect(socket.sent.map((message) => message['action'])).toEqual(['stream-text', 'cancel', 'stream-text', 'end-stream']);
  });

  it('drops cancelled audio and emits only audio from the active context', () => {
    const socket = new FakeDeepdubSocket();
    const session = new DeepdubTtsSession(socket as unknown as WebSocket);
    const audio: string[] = [];
    session.onAudio((event) => audio.push(`${event.contextId}:${event.data}`));
    session.send('old', preparation('ישן'), true);
    session.cancel('old');
    socket.emitMessage({ data: 'b2xk', isFinished: false });
    expect(audio).toEqual([]);
    socket.emitMessage({ isCancelled: true, isFinished: true });
    session.send('new', preparation('חדש'), false);
    socket.emitMessage({ data: 'bmV3', isFinished: false });
    expect(audio).toEqual(['new:bmV3']);
  });

  it('closes the provider socket and notifies bridge cleanup', () => {
    const socket = new FakeDeepdubSocket();
    const session = new DeepdubTtsSession(socket as unknown as WebSocket);
    const closed = vi.fn();
    session.onClose(closed);
    session.close();
    expect(closed).toHaveBeenCalledOnce();
    expect(socket.readyState).toBe(WebSocket.CLOSED);
  });
});

describe('pronunciation persistence and observability', () => {
  it('scopes list and delete operations by business', async () => {
    const store = new MemoryStore();
    const created = await store.createPronunciation(firstBusinessId, { sourceText: 'MAX', pronunciation: 'מקס', pronunciationType: 'replacement', locale: 'he-IL' });
    expect(await store.listPronunciations(secondBusinessId)).toEqual([]);
    expect(await store.deletePronunciation(secondBusinessId, created.id)).toBe(false);
    expect(await store.deletePronunciation(firstBusinessId, created.id)).toBe(true);
  });

  it('exports structured event and stage latency metrics', () => {
    const metrics = new MetricsRegistry();
    metrics.event('deepdub', 'pronunciation_cache_hit');
    metrics.timing('deepdub', 'speech_end_to_twilio_audio', 339, { source: 'cache' });
    const rendered = metrics.render();
    expect(rendered).toContain('callora_realtime_events_total{event="pronunciation_cache_hit",provider="deepdub"} 1');
    expect(rendered).toContain('callora_realtime_stage_latency_ms_count{provider="deepdub",source="cache",stage="speech_end_to_twilio_audio"} 1');
  });

  it('ships at least 100 categorized listening utterances', async () => {
    const dataset = JSON.parse(await readFile(new URL('../evaluation/hebrew-utterances.json', import.meta.url), 'utf8')) as Record<string, string[]>;
    expect(Object.values(dataset).flat().length).toBeGreaterThanOrEqual(100);
    expect(Object.keys(dataset).length).toBeGreaterThanOrEqual(10);
  });
});

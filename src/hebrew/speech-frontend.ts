import { performance } from 'node:perf_hooks';
import type { HebrewPronunciationMode, PronunciationEntry } from '../domain/models.js';
import { normalizePronunciationKey, normalizeSpokenHebrew } from './normalizer.js';
import type { PronunciationMetrics, SpeechPreparation, SpeechPreprocessor } from './types.js';
import { detectHebrewPronunciationRisk } from './risk-detector.js';
import type { ReNikudClient } from './renikud-client.js';

type CacheValue = Omit<SpeechPreparation, 'processingMs' | 'cacheHit' | 'source'>;

/** Bounded insertion-order cache; keys always include the tenant id. */
export class PronunciationCache {
  private readonly entries = new Map<string, CacheValue>();
  public constructor(private readonly maximumEntries = 2_000) {}
  public get(key: string): CacheValue | undefined {
    const value = this.entries.get(key);
    if (value) {
      this.entries.delete(key);
      this.entries.set(key, value);
    }
    return value;
  }
  public set(key: string, value: CacheValue): void {
    this.entries.set(key, value);
    while (this.entries.size > this.maximumEntries) {
      const oldest = this.entries.keys().next().value as string | undefined;
      if (!oldest) break;
      this.entries.delete(oldest);
    }
  }
}

export const pronunciationCache = new PronunciationCache();

/**
 * English words Israelis say inside Hebrew, spelled the way they say them. A he-IL voice
 * reading the Latin spelling switches to an English accent mid-sentence; the Hebrew
 * spelling keeps it in one voice. A business dictionary entry for the same word wins.
 */
export const BUILT_IN_HEBREW_TRANSLITERATIONS: ReadonlyArray<readonly [string, string]> = [
  ['WhatsApp', 'וואטסאפ'],
  ['PayBox', 'פייבוקס'],
  ['Gmail', "ג'ימייל"],
  ['Google', 'גוגל'],
  ['Facebook', 'פייסבוק'],
  ['Instagram', 'אינסטגרם'],
  ['iPhone', 'אייפון'],
  ['Zoom', 'זום'],
  ['email', 'אימייל'],
  ['SMS', 'אס אם אס'],
];


// Hebrew prefixes (בWhatsApp, ב-PayBox) are fine; only a Latin letter or digit glued on
// means the match is part of a different word. Every entry is plain letters, so it is
// safe to use as a pattern.
const BUILT_IN_PATTERNS = BUILT_IN_HEBREW_TRANSLITERATIONS.map(
  ([word, spoken]) => [word, new RegExp(`(?<![A-Za-z0-9])${word}(?![A-Za-z0-9])`, 'gi'), spoken] as const,
);

function dictionaryFingerprint(dictionary: readonly PronunciationEntry[]): string {
  let hash = 0;
  for (const entry of dictionary) {
    const line = [entry.id, entry.normalizedText, entry.pronunciation, entry.pronunciationType, entry.locale].join('|');
    for (let index = 0; index < line.length; index += 1) hash = (Math.imul(hash, 31) + line.charCodeAt(index)) | 0;
  }
  return `${dictionary.length}:${(hash >>> 0).toString(36)}`;
}

export interface HebrewSpeechFrontendOptions {
  businessId: string;
  locale: string;
  mode: HebrewPronunciationMode;
  dictionary: readonly PronunciationEntry[];
  renikud?: ReNikudClient;
  cache?: PronunciationCache;
  speakerGender?: 'male' | 'female';
  targetGender?: 'male' | 'female';
  metrics?: PronunciationMetrics;
}

export class HebrewSpeechFrontend implements SpeechPreprocessor {
  private readonly cache: PronunciationCache;
  private readonly dictionaryKey: string;
  public constructor(private readonly options: HebrewSpeechFrontendOptions) {
    this.cache = options.cache ?? pronunciationCache;
    this.dictionaryKey = dictionaryFingerprint(options.dictionary);
  }

  public async preprocess(text: string, signal?: AbortSignal): Promise<SpeechPreparation> {
    const started = performance.now();
    this.options.metrics?.pronunciationStarted();
    const originalText = text;
    if (this.options.mode === 'off' || !/[\u0590-\u05FF]/u.test(text)) {
      return this.complete({ originalText, spokenText: text, locale: this.options.locale, mode: this.options.mode, spans: [], source: 'off', riskReasons: [], processingMs: performance.now() - started, cacheHit: false });
    }

    // The dictionary is part of the key: an entry edited in the dashboard must be heard on
    // the very next call, not after the process happens to evict the old result.
    const cacheKey = `${this.options.businessId}\u0000${this.options.locale}\u0000${this.options.mode}\u0000${this.dictionaryKey}\u0000${normalizePronunciationKey(text)}`;
    const cached = this.cache.get(cacheKey);
    if (cached) {
      return this.complete({ ...cached, source: 'cache', processingMs: performance.now() - started, cacheHit: true });
    }

    let spokenText = normalizeSpokenHebrew(text);
    const businessEntries = this.options.dictionary.filter(
      (entry) => entry.locale.toLowerCase() === this.options.locale.toLowerCase() && normalizePronunciationKey(spokenText).includes(entry.normalizedText),
    );
    // Rewritten into the text itself rather than kept as spans, so a clause-level ReNikud
    // pronunciation is computed from the Hebrew spelling instead of swallowing it.
    for (const [word, pattern, spoken] of BUILT_IN_PATTERNS) {
      if (!businessEntries.some((entry) => entry.normalizedText === normalizePronunciationKey(word))) spokenText = spokenText.replace(pattern, spoken);
    }
    const spans: SpeechPreparation['spans'] = businessEntries.map((entry) => ({ sourceText: entry.sourceText, pronunciation: entry.pronunciation, type: entry.pronunciationType }));
    const risk = detectHebrewPronunciationRisk(spokenText, businessEntries.length > 0);
    let source: SpeechPreparation['source'] = spans.length > 0 ? 'dictionary' : 'native';

    // ReNikud returns one pronunciation for the whole clause, which would overwrite what the
    // business chose for a word inside it; a business entry therefore always wins.
    const shouldUseReNikud = Boolean(
      this.options.renikud &&
      businessEntries.length === 0 &&
      (this.options.mode === 'strict' || (this.options.mode === 'smart' && risk.risky)),
    );
    if (shouldUseReNikud && !signal?.aborted) {
      const result = await this.options.renikud!.pronounce(
        spokenText,
        { ...(this.options.speakerGender ? { speakerGender: this.options.speakerGender } : {}), ...(this.options.targetGender ? { targetGender: this.options.targetGender } : {}) },
        signal,
      );
      if (result) {
        spans.push({ sourceText: spokenText, pronunciation: result.pronunciation, type: 'ipa' });
        source = 'renikud';
      } else {
        source = spans.length > 0 ? 'dictionary' : 'fallback';
      }
    }

    const stable: CacheValue = { originalText, spokenText, locale: this.options.locale, mode: this.options.mode, spans, riskReasons: risk.reasons };
    this.cache.set(cacheKey, stable);
    return this.complete({ ...stable, source, processingMs: performance.now() - started, cacheHit: false });
  }

  private complete(result: SpeechPreparation): SpeechPreparation {
    this.options.metrics?.pronunciationCompleted(result);
    return result;
  }
}

export class PassthroughSpeechPreprocessor implements SpeechPreprocessor {
  public async preprocess(text: string): Promise<SpeechPreparation> {
    return { originalText: text, spokenText: text, locale: '', mode: 'off', spans: [], source: 'off', riskReasons: [], processingMs: 0, cacheHit: false };
  }
}

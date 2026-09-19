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
  public constructor(private readonly options: HebrewSpeechFrontendOptions) {
    this.cache = options.cache ?? pronunciationCache;
  }

  public async preprocess(text: string, signal?: AbortSignal): Promise<SpeechPreparation> {
    const started = performance.now();
    this.options.metrics?.pronunciationStarted();
    const originalText = text;
    if (this.options.mode === 'off' || !/[\u0590-\u05FF]/u.test(text)) {
      return this.complete({ originalText, spokenText: text, locale: this.options.locale, mode: this.options.mode, spans: [], source: 'off', riskReasons: [], processingMs: performance.now() - started, cacheHit: false });
    }

    const cacheKey = `${this.options.businessId}\u0000${this.options.locale}\u0000${this.options.mode}\u0000${normalizePronunciationKey(text)}`;
    const cached = this.cache.get(cacheKey);
    if (cached) {
      return this.complete({ ...cached, source: 'cache', processingMs: performance.now() - started, cacheHit: true });
    }

    const spokenText = normalizeSpokenHebrew(text);
    const spans = this.options.dictionary
      .filter((entry) => entry.locale.toLowerCase() === this.options.locale.toLowerCase() && normalizePronunciationKey(spokenText).includes(entry.normalizedText))
      .map((entry) => ({ sourceText: entry.sourceText, pronunciation: entry.pronunciation, type: entry.pronunciationType }));
    const risk = detectHebrewPronunciationRisk(spokenText, spans.length > 0);
    let source: SpeechPreparation['source'] = spans.length > 0 ? 'dictionary' : 'native';

    const shouldUseReNikud = Boolean(
      this.options.renikud &&
      (this.options.mode === 'strict' || (this.options.mode === 'smart' && risk.risky && spans.length === 0)),
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

import type { HebrewPronunciationMode, PronunciationType } from '../domain/models.js';

export interface PronunciationSpan {
  sourceText: string;
  pronunciation: string;
  type: PronunciationType;
}

export type PronunciationSource = 'off' | 'native' | 'dictionary' | 'cache' | 'renikud' | 'fallback';

/** Provider-neutral output. Provider adapters decide how IPA is represented on the wire. */
export interface SpeechPreparation {
  originalText: string;
  spokenText: string;
  locale: string;
  mode: HebrewPronunciationMode;
  spans: PronunciationSpan[];
  source: PronunciationSource;
  riskReasons: string[];
  processingMs: number;
  cacheHit: boolean;
}

export interface SpeechPreprocessor {
  preprocess(text: string, signal?: AbortSignal): Promise<SpeechPreparation>;
}

export interface PronunciationMetrics {
  pronunciationStarted(): void;
  pronunciationCompleted(result: SpeechPreparation): void;
}


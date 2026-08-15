/**
 * Cartesia defaults, kept apart from the protocol builders so `config.ts` can import
 * them without pulling in the wire format.
 *
 * Every one of these is overridable by environment variable. Model identifiers and the
 * API version are the values most likely to move under us, and a wrong one is only
 * discoverable on a live call, so being able to correct them without shipping code is
 * deliberate.
 */

/**
 * Sonic TTS model, pinned to a dated snapshot as Cartesia recommends for production.
 * Hebrew requires the sonic-3 or sonic-3.5 family; sonic-2 and sonic-turbo do not carry
 * it and are scheduled to sunset. Override with `CARTESIA_TTS_MODEL`.
 */
export const DEFAULT_CARTESIA_TTS_MODEL = 'sonic-3.5-2026-05-04' as const;

/**
 * Streaming STT model. `ink-whisper` is the multilingual one; `ink-2` is faster but
 * English-only, so it cannot serve a Hebrew tenant. Override with `CARTESIA_STT_MODEL`.
 */
export const DEFAULT_CARTESIA_STT_MODEL = 'ink-whisper' as const;

/** Cartesia pins breaking changes behind a dated version, sent as a query parameter. */
export const CARTESIA_API_VERSION = '2026-03-01' as const;

/** Reasoning model for the Cartesia pipeline; Cartesia itself provides no LLM. */
export const DEFAULT_TEXT_LLM_MODEL = 'gpt-4o-mini' as const;

/**
 * G.711 mu-law at 8 kHz, exactly what Twilio Media Streams carry, so audio crosses this
 * provider untranscoded in both directions.
 */
export const CARTESIA_AUDIO_ENCODING = 'pcm_mulaw' as const;
export const CARTESIA_SAMPLE_RATE = 8000 as const;
export const CARTESIA_CONTAINER = 'raw' as const;

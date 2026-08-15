/**
 * The realtime speech-to-speech backends Callora can bridge a Twilio call to.
 *
 * Exactly one is active per deployment, chosen by `VOICE_PROVIDER`. All of them carry
 * G.711 mu-law at 8 kHz in and out, so no path transcodes Twilio audio.
 *
 * `openai` and `elevenlabs` are single-vendor speech-to-speech sessions. `cartesia` is
 * assembled from three streams — Cartesia STT, a text LLM, and Cartesia Sonic TTS — and
 * is the only provider where Callora itself owns the turn loop.
 */
export const REALTIME_PROVIDERS = ['openai', 'elevenlabs', 'cartesia'] as const;

export type RealtimeProvider = (typeof REALTIME_PROVIDERS)[number];

export const DEFAULT_REALTIME_PROVIDER: RealtimeProvider = 'openai';

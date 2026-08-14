/**
 * The realtime speech-to-speech backends Callora can bridge a Twilio call to.
 *
 * Exactly one is active per deployment, chosen by `VOICE_PROVIDER`. Both bridge
 * G.711 mu-law at 8 kHz in and out, so neither path transcodes Twilio audio.
 */
export const REALTIME_PROVIDERS = ['openai', 'elevenlabs'] as const;

export type RealtimeProvider = (typeof REALTIME_PROVIDERS)[number];

export const DEFAULT_REALTIME_PROVIDER: RealtimeProvider = 'openai';

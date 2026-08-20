/**
 * How a Callora agent should be voiced on ElevenLabs.
 *
 * These are the settings that decide whether a call sounds like a person or like a
 * text-to-speech demo, and until now Callora never wrote any of them: they stayed on
 * whatever the ElevenLabs dashboard happened to hold, which made Callora the source of
 * truth for the words and not for the delivery. Choosing them here is what makes a saved
 * agent sound the same on every deployment.
 */

/** The only two models that can speak every language Callora offers, Hebrew included. */
export const CONVERSATIONAL_TTS_MODEL = 'eleven_v3_conversational' as const;
export const EXPRESSIVE_TTS_MODEL = 'eleven_v3' as const;

/** Fast models, which are English-only and therefore not a default Callora can pick. */
export const LATENCY_TTS_MODEL = 'eleven_flash_v2_5' as const;

/**
 * Languages the fast models cannot speak.
 *
 * ElevenLabs reports this per model, and rejects an agent whose language the model does
 * not support — but only when the language is written onto the agent, which is a long way
 * from where an operator picked the model. Hebrew is the case this platform hit: on
 * `eleven_flash_v2` it produced audible nonsense rather than an error.
 */
export function ttsModelSupportsLanguage(model: string, language: string | undefined): boolean {
  if (!language) return true;
  const v3 = model === CONVERSATIONAL_TTS_MODEL || model === EXPRESSIVE_TTS_MODEL;
  return v3 || !V3_ONLY_LANGUAGES.has(language);
}

/** ISO-639-1 codes served only by the v3 models. */
export const V3_ONLY_LANGUAGES: ReadonlySet<string> = new Set(['he']);

/**
 * Voice settings tuned for a phone conversation rather than a narration.
 *
 * `stability` is the one that matters most: at the ElevenLabs default of 0.5 the delivery
 * is even and flat, which reads as synthetic across a whole call. Lower widens the
 * emotional range at the cost of some consistency, which is the right trade for speech
 * that is meant to sound spontaneous.
 */
export const PHONE_VOICE_SETTINGS = {
  stability: 0.35,
  similarityBoost: 0.75,
  speed: 1.0,
} as const;

/**
 * Latency optimisation degrades prosody, and on this model it is not needed: the first
 * audio chunk already arrives well inside what a caller reads as a normal pause. Zero is
 * the setting that keeps the delivery intact.
 */
export const STREAMING_LATENCY_OPTIMISATION = 0 as const;

/**
 * Turn handling.
 *
 * `silenceEndCallTimeout` is the fix for the failure the transcripts show: with the
 * ElevenLabs default of -1 a call never ends on its own, so an agent facing a caller whose
 * speech does not transcribe asks the same question until the duration ceiling. Ending
 * after a clear stretch of silence is what a person does.
 */
export const TURN_SETTINGS = {
  turnTimeoutSeconds: 5,
  silenceEndCallTimeoutSeconds: 25,
} as const;

/**
 * The TTS model Callora writes.
 *
 * One model for every tenant, deliberately: the fast models are English-only, and a
 * platform that quietly picked a different model per language would sound like a
 * different product depending on who called.
 */
export function defaultTtsModel(): string {
  return CONVERSATIONAL_TTS_MODEL;
}

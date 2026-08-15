import type { AgentConfig } from '../domain/models.js';
import {
  CARTESIA_AUDIO_ENCODING,
  CARTESIA_CONTAINER,
  CARTESIA_SAMPLE_RATE,
} from './cartesia-constants.js';
import { languageCode } from './policy.js';

/**
 * Payload builders for the two Cartesia WebSockets.
 *
 * Both carry G.711 mu-law at 8 kHz — `pcm_mulaw` / `8000` in Cartesia's vocabulary,
 * which is exactly what Twilio Media Streams already contain — so audio crosses this
 * provider untranscoded in both directions. The only transformation anywhere is base64,
 * because Twilio wraps its mu-law in JSON while Cartesia STT takes raw binary frames.
 */

/** STT commands are bare text frames, deliberately not JSON. */
export const STT_FINALIZE_COMMAND = 'finalize' as const;
export const STT_CLOSE_COMMAND = 'close' as const;

export const CARTESIA_OUTPUT_FORMAT = {
  container: CARTESIA_CONTAINER,
  encoding: CARTESIA_AUDIO_ENCODING,
  sample_rate: CARTESIA_SAMPLE_RATE,
} as const;

/**
 * Cartesia expects a bare ISO-639-1 code, so a `he-IL` tenant becomes `he`.
 * Falls back to the agent locale's own code and never invents one.
 */
export function cartesiaLanguage(agent: AgentConfig): string | undefined {
  return languageCode(agent.language);
}

export interface SttUrlOptions {
  baseUrl: string;
  model: string;
  version: string;
  language?: string | undefined;
}

/** Query parameters are the only way to configure the STT socket; there is no init message. */
export function buildSttUrl(options: SttUrlOptions): string {
  const url = new URL(`${options.baseUrl}/stt/websocket`);
  url.searchParams.set('model', options.model);
  url.searchParams.set('encoding', CARTESIA_AUDIO_ENCODING);
  url.searchParams.set('sample_rate', String(CARTESIA_SAMPLE_RATE));
  url.searchParams.set('cartesia_version', options.version);
  if (options.language) {
    url.searchParams.set('language', options.language);
  }
  return url.toString();
}

export function buildTtsUrl(options: { baseUrl: string; version: string }): string {
  const url = new URL(`${options.baseUrl}/tts/websocket`);
  url.searchParams.set('cartesia_version', options.version);
  return url.toString();
}

export interface TtsChunkOptions {
  model: string;
  voiceId: string;
  contextId: string;
  transcript: string;
  language?: string | undefined;
  /** True while more of this turn is still coming from the LLM. */
  continue: boolean;
}

/**
 * One transcript fragment on a context.
 *
 * Fragments sharing a `context_id` are synthesised as one continuous utterance with
 * prosody carried across them, which is what makes it safe to push LLM deltas straight
 * in rather than waiting for the whole reply.
 */
export function buildTtsChunk(options: TtsChunkOptions): Record<string, unknown> {
  return {
    model_id: options.model,
    transcript: options.transcript,
    voice: { mode: 'id', id: options.voiceId },
    ...(options.language ? { language: options.language } : {}),
    context_id: options.contextId,
    output_format: CARTESIA_OUTPUT_FORMAT,
    continue: options.continue,
  };
}

/**
 * Abandons a context.
 *
 * Cartesia documents this as halting only requests that have **not started generating**;
 * anything already in flight keeps streaming chunks until it finishes. Barge-in therefore
 * cannot rely on this alone — the bridge also stops forwarding chunks whose `context_id`
 * is no longer the active one, and clears what Twilio has buffered.
 */
export function buildTtsCancel(contextId: string): Record<string, unknown> {
  return { context_id: contextId, cancel: true };
}

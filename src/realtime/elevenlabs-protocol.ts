import type { AgentConfig } from '../domain/models.js';
import { composeAgentInstructions, languageCode } from './policy.js';
import { END_CALL_TOOL_NAME } from './protocol.js';

/**
 * Payload builders for the ElevenLabs Agents WebSocket API.
 *
 * Twilio phone audio is G.711 mu-law at 8 kHz, which ElevenLabs calls `ulaw_8000` and
 * supports for both agent output and user input, so this bridge transcodes nothing.
 * The format itself is configured on the agent in the ElevenLabs dashboard; the session
 * reports back what it actually chose in `conversation_initiation_metadata`, which the
 * bridge checks rather than assumes.
 */

export const ELEVENLABS_AUDIO_FORMAT = 'ulaw_8000' as const;

/** Path of the short-lived signed URL used to open an authenticated agent session. */
export const SIGNED_URL_PATH = '/v1/convai/conversation/get-signed-url' as const;

/**
 * Per-call overrides sent as the very first client message.
 *
 * The prompt, first message, and language are the same tenant `AgentConfig` fields the
 * OpenAI path uses, so both providers speak with one policy and one greeting. Each of
 * these must be enabled individually under the agent's Security tab in the ElevenLabs
 * dashboard: an override for a field that is not enabled is rejected with an error
 * rather than ignored.
 */
export interface ConversationOverrides {
  /** Bare ISO-639-1 code, e.g. `he` for a `he-IL` agent; absent when the locale is unusable. */
  language: string | undefined;
  /** The tenant greeting, verbatim. Empty means the tenant has not configured one. */
  firstMessage: string;
  prompt: string;
  /** Voice id chosen in Callora; absent keeps the agent's own configured voice. */
  voiceId: string | undefined;
}

/**
 * Resolves what Callora will override for this call.
 *
 * Split out from the payload builder so the bridge can log exactly the values it sends:
 * anything derived twice would eventually disagree, and the whole point of the log line
 * is to be able to trust it when a call comes out in the wrong language.
 *
 * ElevenLabs expects a bare language code, so a `he-IL` agent is narrowed to `he`.
 */
export function resolveConversationOverrides(options: {
  agent: AgentConfig;
  callerNumber?: string | null;
  voiceId?: string;
}): ConversationOverrides {
  const { agent, callerNumber, voiceId } = options;
  return {
    language: languageCode(agent.language),
    firstMessage: agent.greeting.trim(),
    prompt: composeAgentInstructions({ agent, callerNumber }),
    voiceId: voiceId?.trim() ? voiceId.trim() : undefined,
  };
}

export function buildConversationInitiation(options: {
  agent: AgentConfig;
  callerNumber?: string | null;
  voiceId?: string;
}): Record<string, unknown> {
  const { language, firstMessage, prompt, voiceId } = resolveConversationOverrides(options);

  return {
    type: 'conversation_initiation_client_data',
    conversation_config_override: {
      agent: {
        prompt: { prompt },
        // Always sent when the tenant has a greeting, so the agent can never open with
        // the English first message configured on the ElevenLabs side.
        ...(firstMessage ? { first_message: firstMessage } : {}),
        ...(language ? { language } : {}),
      },
      // Sent only when Callora holds a voice for this business, so an agent whose voice
      // override is not enabled in the ElevenLabs dashboard is not rejected needlessly.
      ...(voiceId ? { tts: { voice_id: voiceId } } : {}),
    },
  };
}

/** Caller audio, forwarded frame for frame. Deliberately not a `type`-tagged message. */
export function buildUserAudioChunk(payload: string): Record<string, unknown> {
  return { user_audio_chunk: payload };
}

/** Keepalive answer; ElevenLabs closes the socket if pings go unanswered. */
export function buildPong(eventId: number): Record<string, unknown> {
  return { type: 'pong', event_id: eventId };
}

/** Answers a client tool call so the agent's turn can continue or close cleanly. */
export function buildClientToolResult(
  toolCallId: string,
  result: string,
  isError = false,
): Record<string, unknown> {
  return { type: 'client_tool_result', tool_call_id: toolCallId, result, is_error: isError };
}

/**
 * The client tool the agent calls to hang up, registered under the same name the OpenAI
 * path uses so the tenant-facing policy text stays identical across providers.
 */
export const ELEVENLABS_END_CALL_TOOL_NAME = END_CALL_TOOL_NAME;

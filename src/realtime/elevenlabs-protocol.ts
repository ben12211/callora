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
export function buildConversationInitiation(options: {
  agent: AgentConfig;
  callerNumber?: string | null;
}): Record<string, unknown> {
  const { agent, callerNumber } = options;
  const language = languageCode(agent.language);

  return {
    type: 'conversation_initiation_client_data',
    conversation_config_override: {
      agent: {
        prompt: { prompt: composeAgentInstructions({ agent, callerNumber }) },
        first_message: agent.greeting,
        ...(language ? { language } : {}),
      },
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

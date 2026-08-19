import type { AgentConfig } from '../domain/models.js';
import { composeAgentInstructions, languageCode } from './policy.js';

/**
 * Writes a business's configuration into its ElevenLabs agent.
 *
 * Per-call conversation overrides only apply when the matching switch is enabled under the
 * agent's Security tab, and ElevenLabs rejects the whole conversation when one is not —
 * which is indistinguishable, from the caller's side, from Callora ignoring the dashboard.
 * The model cannot be overridden per call at all. Writing the configuration onto the agent
 * removes both problems: what an operator saves in Callora becomes what the ElevenLabs
 * agent itself is configured with, and the overrides then merely restate it.
 *
 * The update is read-modify-write. Only the leaves Callora owns are replaced, so
 * everything else configured on the ElevenLabs side — tools, knowledge base, evaluation,
 * anything this build has never heard of — survives untouched.
 */

export const AGENT_PATH = '/v1/convai/agents' as const;

export interface ElevenLabsManagementOptions {
  apiKey: string;
  /** API origin, without a trailing slash. */
  baseUrl: string;
  agentId: string;
  /** Injectable so tests never reach the ElevenLabs API. */
  fetchImpl?: typeof fetch;
}

/** The fields Callora owns on the remote agent. */
export interface AgentConfiguration {
  prompt: string;
  firstMessage: string;
  /** Bare ISO-639-1 code; omitted when the locale is not one Callora can narrow. */
  language?: string;
  /** The reasoning model. On this provider it is only meaningful once written here. */
  llmModel?: string;
  voiceId?: string;
}

export type SyncResult =
  | { ok: true; agentId: string }
  | { ok: false; agentId: string; status?: number; message: string };

export function agentConfigurationFor(agent: AgentConfig): AgentConfiguration {
  const language = languageCode(agent.language);
  const llmModel = agent.realtimeModel.trim();
  const voiceId = agent.voice.trim();
  return {
    prompt: composeAgentInstructions({ agent }),
    firstMessage: agent.greeting.trim(),
    ...(language ? { language } : {}),
    ...(llmModel ? { llmModel } : {}),
    ...(voiceId ? { voiceId } : {}),
  };
}

function asRecord(value: unknown): Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
    ? { ...(value as Record<string, unknown>) }
    : {};
}

/**
 * Merges Callora's fields into the agent's existing configuration.
 *
 * Exported for the tests, which is the only way to be sure the merge preserves the parts
 * of a real agent this code does not model.
 */
export function mergeAgentConfiguration(
  existing: unknown,
  update: AgentConfiguration,
): Record<string, unknown> {
  const conversation = asRecord(existing);
  const agent = asRecord(conversation['agent']);
  const prompt = asRecord(agent['prompt']);
  const tts = asRecord(conversation['tts']);

  prompt['prompt'] = update.prompt;
  if (update.llmModel) {
    prompt['llm'] = update.llmModel;
  }
  agent['prompt'] = prompt;
  agent['first_message'] = update.firstMessage;
  if (update.language) {
    agent['language'] = update.language;
  }
  conversation['agent'] = agent;

  if (update.voiceId) {
    tts['voice_id'] = update.voiceId;
    conversation['tts'] = tts;
  }

  return conversation;
}

/** Short, non-secret description of a failed request; never echoes the key or the body. */
function failure(agentId: string, status: number | undefined, message: string): SyncResult {
  return { ok: false, agentId, ...(status === undefined ? {} : { status }), message };
}

/**
 * Pushes the configuration onto the agent.
 *
 * Never throws: a provider that is down, renamed a field, or rejected the key must show up
 * as a message next to the form, not as a failed save of configuration that is already
 * stored correctly in Callora.
 */
export async function pushAgentConfiguration(
  options: ElevenLabsManagementOptions & { update: AgentConfiguration },
): Promise<SyncResult> {
  const { apiKey, baseUrl, agentId, update, fetchImpl = fetch } = options;
  const url = `${baseUrl}${AGENT_PATH}/${encodeURIComponent(agentId)}`;
  const headers = { 'xi-api-key': apiKey, 'content-type': 'application/json' };

  let existing: unknown;
  try {
    const current = await fetchImpl(url, { headers: { 'xi-api-key': apiKey } });
    if (!current.ok) {
      return failure(
        agentId,
        current.status,
        current.status === 404
          ? 'ElevenLabs has no agent with that id.'
          : 'ElevenLabs refused to return the agent.',
      );
    }
    existing = asRecord(await current.json())['conversation_config'];
  } catch (error) {
    return failure(agentId, undefined, `Could not reach ElevenLabs: ${errorText(error)}`);
  }

  try {
    const response = await fetchImpl(url, {
      method: 'PATCH',
      headers,
      body: JSON.stringify({ conversation_config: mergeAgentConfiguration(existing, update) }),
    });
    if (!response.ok) {
      return failure(agentId, response.status, 'ElevenLabs rejected the update.');
    }
  } catch (error) {
    return failure(agentId, undefined, `Could not reach ElevenLabs: ${errorText(error)}`);
  }

  return { ok: true, agentId };
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : 'unknown error';
}

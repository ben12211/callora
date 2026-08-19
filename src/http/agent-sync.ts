import type { ProviderCredentials } from '../config.js';
import type { AgentConfig } from '../domain/models.js';
import {
  agentConfigurationFor,
  pushAgentConfiguration,
  type SyncResult,
} from '../realtime/elevenlabs-management.js';

/**
 * Pushes a saved agent configuration into the provider that executes it.
 *
 * Only ElevenLabs needs this. OpenAI and Cartesia are configured entirely per call, so
 * what is stored in Callora is already what the next call uses. ElevenLabs keeps its own
 * copy of the prompt, first message, language, voice, and model, and per-call overrides
 * only reach it when each one is enabled on that agent — so for the dashboard to be the
 * source of truth there, Callora has to write into the agent itself.
 *
 * A push never fails the save. The configuration is stored in Callora either way; what a
 * failure means is that ElevenLabs is still running the previous copy, and the operator
 * needs to be told that in the same breath as "saved".
 */

export interface AgentSyncOutcome {
  /** True when nothing needed pushing, which is the normal case for two of three providers. */
  skipped: boolean;
  /** Sentence to show next to the save confirmation. */
  message: string;
  ok: boolean;
  result?: SyncResult;
}

export interface AgentSyncOptions {
  agent: AgentConfig;
  providers: ProviderCredentials;
  /** Injectable so tests never reach the ElevenLabs API. */
  fetchImpl?: typeof fetch;
}

export async function syncAgentToProvider(options: AgentSyncOptions): Promise<AgentSyncOutcome> {
  const { agent, providers, fetchImpl } = options;

  if (agent.voiceProvider !== 'elevenlabs') {
    return { skipped: true, ok: true, message: '' };
  }

  const credentials = providers.elevenlabs;
  if (!credentials) {
    return {
      skipped: true,
      ok: false,
      message:
        'Nothing was sent to ElevenLabs: this platform has no ElevenLabs credentials. Add them on the Providers page.',
    };
  }

  const agentId = agent.elevenLabsAgentId.trim();
  if (!agentId) {
    return {
      skipped: true,
      ok: false,
      message:
        'Nothing was sent to ElevenLabs: this business has no agent of its own. Give it an ElevenLabs agent id, or it keeps running on the shared platform agent, which Callora never overwrites.',
    };
  }

  const result = await pushAgentConfiguration({
    apiKey: credentials.apiKey,
    baseUrl: credentials.apiBaseUrl,
    agentId,
    update: agentConfigurationFor(agent),
    ...(fetchImpl ? { fetchImpl } : {}),
  });

  return result.ok
    ? {
        skipped: false,
        ok: true,
        message: `Pushed to ElevenLabs agent ${agentId}.`,
        result,
      }
    : {
        skipped: false,
        ok: false,
        message: `ElevenLabs still has the previous configuration: ${result.message}${
          result.status ? ` (HTTP ${result.status})` : ''
        }`,
        result,
      };
}

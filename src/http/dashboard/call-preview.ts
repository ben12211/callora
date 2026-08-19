import type { ProviderCredentials } from '../../config.js';
import type { AgentConfig } from '../../domain/models.js';
import { cartesiaLanguage } from '../../realtime/cartesia-protocol.js';
import { buildConversationInitiation } from '../../realtime/elevenlabs-protocol.js';
import { composeAgentInstructions } from '../../realtime/policy.js';
import { buildGreetingResponse, buildSessionUpdate } from '../../realtime/protocol.js';

/**
 * What the next call to this business would actually send.
 *
 * Every value here is resolved by the same code the call path runs, from the same stored
 * agent row and the same platform credentials — never from a copy written for the page.
 * That is the whole point: an operator who is told the agent ignores their configuration
 * can look at this and see either their own words, in which case the configuration is
 * reaching the provider, or something else, in which case this says what and why.
 */

export interface PreviewField {
  label: string;
  value: string;
  /** True when the provider never receives this value, whatever the form shows. */
  ignored?: boolean;
  note?: string;
}

export interface CallPreview {
  providerLabel: string;
  /** False when the platform holds no credentials, so the call answers with the greeting. */
  providerConfigured: boolean;
  enabled: boolean;
  fields: PreviewField[];
  /** The system prompt exactly as the provider receives it. */
  instructions: string;
  /** The first thing the caller hears. */
  greeting: string;
  /** The initiation payload, for providers where Callora sends one. */
  payload?: string;
  payloadNote?: string;
  warnings: string[];
}

/** Pretty-prints a payload for display; it never contains a credential. */
function formatPayload(payload: unknown): string {
  return JSON.stringify(payload, null, 2);
}

export function buildCallPreview(options: {
  agent: AgentConfig;
  providers: ProviderCredentials;
  providerLabel: string;
}): CallPreview {
  const { agent, providers, providerLabel } = options;
  const instructions = composeAgentInstructions({ agent });
  const warnings: string[] = [];

  if (!agent.enabled) {
    warnings.push(
      'This agent is turned off. Calls answer with the business fallback greeting and none of the configuration below is used.',
    );
  }

  if (agent.voiceProvider === 'elevenlabs') {
    const credentials = providers.elevenlabs;
    if (!credentials) {
      warnings.push(
        'The platform holds no ElevenLabs credentials, so calls fall back to the business greeting. Add them on the Providers page.',
      );
    }
    warnings.push(
      'ElevenLabs rejects an override that is not enabled on the agent itself. If the prompt, first message, and language switches under its Security tab are off, ElevenLabs discards everything below and answers with the prompt configured on their side — which looks exactly like Callora ignoring this page.',
    );

    const initiation = buildConversationInitiation({
      agent,
      ...(agent.voice.trim() ? { voiceId: agent.voice.trim() } : {}),
    });

    return {
      providerLabel,
      providerConfigured: credentials !== null,
      enabled: agent.enabled,
      fields: [
        { label: 'Language sent', value: (cartesiaLanguage(agent) ?? 'not sent') },
        {
          label: 'Voice sent',
          value: agent.voice.trim() || 'not sent — keeps the voice configured on the ElevenLabs agent',
        },
        {
          label: 'Model',
          value: agent.realtimeModel,
          ignored: true,
          note: 'Stored in Callora and never sent. The model belongs to the agent in the ElevenLabs dashboard.',
        },
        {
          label: 'ElevenLabs agent id',
          value: credentials?.agentId ?? 'not configured',
          note: 'Platform-wide: every business on this provider runs on this one agent.',
        },
      ],
      instructions,
      greeting: agent.greeting,
      payload: formatPayload(initiation),
      payloadNote: 'Sent as the first message of every conversation.',
      warnings,
    };
  }

  if (agent.voiceProvider === 'cartesia') {
    const credentials = providers.cartesia;
    if (!credentials) {
      warnings.push(
        'The platform holds no Cartesia credentials, so calls fall back to the business greeting. Add them on the Providers page.',
      );
    }
    const voiceId = agent.voice.trim() || credentials?.defaultVoiceId;
    if (!voiceId) {
      warnings.push('No voice id on this agent and no platform default, so a call cannot be answered on Cartesia.');
    }

    return {
      providerLabel,
      providerConfigured: credentials !== null,
      enabled: agent.enabled,
      fields: [
        { label: 'Language sent', value: cartesiaLanguage(agent) ?? 'not sent' },
        { label: 'Voice id', value: voiceId ?? 'missing' },
        { label: 'Reasoning model', value: agent.realtimeModel.trim() || (credentials?.textLlmModel ?? 'unset') },
        {
          label: 'Speech models',
          value: `${credentials?.sttModel ?? 'unset'} in, ${credentials?.ttsModel ?? 'unset'} out`,
          note: 'Platform-wide, set on the Providers page rather than per business.',
        },
      ],
      instructions,
      greeting: agent.greeting,
      warnings,
    };
  }

  const credentials = providers.openai;
  if (!credentials) {
    warnings.push(
      'The platform holds no OpenAI credentials, so calls fall back to the business greeting. Add them on the Providers page.',
    );
  }
  const session = buildSessionUpdate({ agent, ...(credentials ? { transcriptionModel: credentials.transcribeModel } : {}) })[
    'session'
  ] as Record<string, unknown>;
  const audio = session['audio'] as Record<string, Record<string, unknown>>;
  const greetingResponse = buildGreetingResponse(agent)['response'] as Record<string, unknown>;
  const input = audio['input'] as Record<string, Record<string, unknown> | undefined>;
  const transcription = input['transcription'] ?? {};

  return {
    providerLabel,
    providerConfigured: credentials !== null,
    enabled: agent.enabled,
    fields: [
      { label: 'Model', value: String(session['model']) },
      { label: 'Voice', value: String((audio['output'] as Record<string, unknown>)['voice'] || 'not set') },
      {
        label: 'Transcription language',
        value: String(transcription['language'] ?? 'auto-detect'),
        note: 'Conversation log only; it never drives the reply.',
      },
    ],
    instructions,
    greeting: String(greetingResponse['instructions'] ?? agent.greeting),
    warnings,
  };
}

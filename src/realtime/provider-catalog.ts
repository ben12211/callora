import type { ProviderCredentials } from '../config.js';
import { DEFAULT_CARTESIA_TTS_MODEL, DEFAULT_TEXT_LLM_MODEL } from './cartesia-constants.js';
import { REALTIME_PROVIDERS, type RealtimeProvider } from './provider.js';

/**
 * What the control plane needs to know about each execution provider: how to describe it,
 * what the `voice` and `model` fields mean there, and which platform credentials make it
 * usable. Callora owns the selection; the providers only execute.
 */
export interface ProviderDescriptor {
  id: RealtimeProvider;
  label: string;
  summary: string;
  /** How the agent's `voice` field is interpreted by this provider. */
  voiceHint: string;
  /** How the agent's `model` field is interpreted by this provider. */
  modelHint: string;
  /** Suggested values offered in the dashboard; free text is still accepted. */
  suggestedVoices: readonly string[];
  suggestedModels: readonly string[];
  /** Environment variables the platform operator sets to enable this provider. */
  requiredEnvironment: readonly string[];
}

export const PROVIDER_CATALOG: Record<RealtimeProvider, ProviderDescriptor> = {
  openai: {
    id: 'openai',
    label: 'OpenAI Realtime',
    summary: 'Single-vendor speech-to-speech session over the OpenAI Realtime API.',
    voiceHint: 'Realtime voice name, for example marin or cedar.',
    modelHint: 'Realtime model snapshot, for example gpt-realtime-2.1.',
    suggestedVoices: ['marin', 'cedar', 'alloy', 'verse'],
    suggestedModels: ['gpt-realtime-2.1', 'gpt-realtime'],
    requiredEnvironment: ['OPENAI_API_KEY'],
  },
  elevenlabs: {
    id: 'elevenlabs',
    label: 'ElevenLabs Agents',
    summary: 'ElevenLabs Agents conversation driven by per-call Callora overrides.',
    voiceHint: 'ElevenLabs voice id; leave blank to keep the voice configured on the agent.',
    // The reasoning model, not the speech model. Saving writes it onto the agent as its
    // LLM; the speech model is chosen by Callora and is not an operator's decision,
    // because the fast ElevenLabs speech models cannot speak every language on offer.
    modelHint:
      'Reasoning model the ElevenLabs agent runs on, for example gpt-5.6-terra. The voice model is chosen by Callora.',
    suggestedVoices: [],
    suggestedModels: ['gpt-5.6-terra', 'gemini-2.5-flash'],
    requiredEnvironment: ['ELEVENLABS_API_KEY', 'ELEVENLABS_AGENT_ID'],
  },
  cartesia: {
    id: 'cartesia',
    label: 'Cartesia',
    summary: 'Cartesia STT and Sonic TTS with a text LLM turn that Callora drives itself.',
    voiceHint: 'Cartesia Sonic voice UUID; blank falls back to CARTESIA_VOICE_ID.',
    modelHint: 'Reasoning model for the text turn, for example gpt-4o-mini.',
    suggestedVoices: [],
    suggestedModels: [DEFAULT_TEXT_LLM_MODEL, DEFAULT_CARTESIA_TTS_MODEL],
    requiredEnvironment: ['CARTESIA_API_KEY', 'OPENAI_API_KEY'],
  },
};

export interface ProviderStatus extends ProviderDescriptor {
  /** True when the platform holds every credential this provider needs. */
  configured: boolean;
  /** Environment variables this deployment is still missing. */
  missingEnvironment: readonly string[];
  /** Non-secret details worth showing an operator, never a credential. */
  details: Record<string, string>;
  isPlatformDefault: boolean;
}

/**
 * Reports which providers this deployment can actually run. Credentials themselves are
 * never included: the dashboard shows availability, not secrets.
 */
export function providerStatuses(
  credentials: ProviderCredentials,
  platformDefault: RealtimeProvider,
  environment: NodeJS.ProcessEnv = process.env,
): ProviderStatus[] {
  const openai = credentials.openai;
  const elevenlabs = credentials.elevenlabs;
  const cartesia = credentials.cartesia;

  return REALTIME_PROVIDERS.map((id) => {
    const descriptor = PROVIDER_CATALOG[id];
    const details: Record<string, string> = {};
    let configured = false;

    if (id === 'openai' && openai) {
      configured = true;
      details['Realtime endpoint'] = openai.realtimeUrl;
      details['Transcription model'] = openai.transcribeModel;
    }
    if (id === 'elevenlabs' && elevenlabs) {
      configured = true;
      details['Agent id'] = elevenlabs.agentId;
      details['API base URL'] = elevenlabs.apiBaseUrl;
    }
    if (id === 'cartesia' && cartesia) {
      configured = true;
      details['TTS model'] = cartesia.ttsModel;
      details['STT model'] = cartesia.sttModel;
      details['API version'] = cartesia.version;
      details['Default voice id'] = cartesia.defaultVoiceId ?? 'not set';
      details['Reasoning model'] = cartesia.textLlmModel;
    }

    return {
      ...descriptor,
      configured,
      missingEnvironment: configured
        ? []
        : descriptor.requiredEnvironment.filter((name) => !environment[name]?.trim()),
      details,
      isPlatformDefault: id === platformDefault,
    };
  });
}

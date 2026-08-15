import 'dotenv/config';
import { z } from 'zod';
import {
  CARTESIA_API_VERSION,
  DEFAULT_CARTESIA_STT_MODEL,
  DEFAULT_CARTESIA_TTS_MODEL,
  DEFAULT_TEXT_LLM_MODEL,
} from './realtime/cartesia-constants.js';
import { DEFAULT_TRANSCRIPTION_MODEL } from './realtime/protocol.js';
import { DEFAULT_REALTIME_PROVIDER, REALTIME_PROVIDERS, type RealtimeProvider } from './realtime/provider.js';

/**
 * Docker Compose and the deployment secret sync always define every provider variable,
 * writing the unused provider's credentials as an empty string rather than omitting
 * them. Zod's `.optional()` admits only `undefined`, so an empty value would otherwise
 * fail `.min(1)` and crash the backend at startup on a perfectly valid single-provider
 * deployment. Treat blank as absent, which is what an empty variable means here.
 */
function blankAsAbsent<T extends z.ZodTypeAny>(schema: T) {
  return z.preprocess((value) => (typeof value === 'string' && value.trim() === '' ? undefined : value), schema);
}

const configSchema = z
  .object({
    NODE_ENV: z.enum(['development', 'test', 'production']).default('development'),
    HOST: z.string().min(1).default('0.0.0.0'),
    PORT: z.coerce.number().int().min(1).max(65535).default(3000),
    LOG_LEVEL: z.enum(['fatal', 'error', 'warn', 'info', 'debug', 'trace', 'silent']).default('info'),
    DATABASE_URL: z.string().min(1),
    TWILIO_AUTH_TOKEN: z.string().min(1),
    // Required: the REST client that hangs calls up is authenticated per account.
    TWILIO_ACCOUNT_SID: z.string().regex(/^AC[0-9a-fA-F]{32}$/, 'TWILIO_ACCOUNT_SID must be a Twilio Account SID'),
    PUBLIC_BASE_URL: z.url().transform((value) => value.replace(/\/$/, '')),
    // Which realtime backend answers calls. Only the selected provider's credentials are required.
    VOICE_PROVIDER: blankAsAbsent(z.enum([...REALTIME_PROVIDERS]).default(DEFAULT_REALTIME_PROVIDER)),
    OPENAI_API_KEY: blankAsAbsent(z.string().min(1).optional()),
    OPENAI_REALTIME_URL: blankAsAbsent(z.string().min(1).default('wss://api.openai.com/v1/realtime')),
    // Transcribes caller audio for conversation logging only; it never drives the reply.
    OPENAI_TRANSCRIBE_MODEL: blankAsAbsent(z.string().min(1).default(DEFAULT_TRANSCRIPTION_MODEL)),
    ELEVENLABS_API_KEY: blankAsAbsent(z.string().min(1).optional()),
    ELEVENLABS_AGENT_ID: blankAsAbsent(z.string().min(1).optional()),
    ELEVENLABS_API_BASE_URL: blankAsAbsent(z.string().min(1).default('https://api.elevenlabs.io')),
    CARTESIA_API_KEY: blankAsAbsent(z.string().min(1).optional()),
    CARTESIA_VOICE_ID: blankAsAbsent(z.string().min(1).optional()),
    CARTESIA_TTS_MODEL: blankAsAbsent(z.string().min(1).default(DEFAULT_CARTESIA_TTS_MODEL)),
    CARTESIA_STT_MODEL: blankAsAbsent(z.string().min(1).default(DEFAULT_CARTESIA_STT_MODEL)),
    CARTESIA_VERSION: blankAsAbsent(z.string().min(1).default(CARTESIA_API_VERSION)),
    CARTESIA_WS_BASE_URL: blankAsAbsent(z.string().min(1).default('wss://api.cartesia.ai')),
    // The Cartesia pipeline supplies its own reasoning through a text LLM rather than a
    // speech-to-speech model, so the chat endpoint is configurable independently.
    TEXT_LLM_MODEL: blankAsAbsent(z.string().min(1).default(DEFAULT_TEXT_LLM_MODEL)),
    TEXT_LLM_BASE_URL: blankAsAbsent(z.string().min(1).default('https://api.openai.com/v1')),
  })
  .superRefine((value, ctx) => {
    // A deployment only needs credentials for the provider it actually uses, so an
    // OpenAI-only server never has to invent an ElevenLabs key and vice versa.
    const byProvider = {
      openai: [['OPENAI_API_KEY', value.OPENAI_API_KEY]],
      elevenlabs: [
        ['ELEVENLABS_API_KEY', value.ELEVENLABS_API_KEY],
        ['ELEVENLABS_AGENT_ID', value.ELEVENLABS_AGENT_ID],
      ],
      // Cartesia covers speech only; the reasoning turn reuses the server-side OpenAI
      // credentials, so that key is required here too.
      cartesia: [
        ['CARTESIA_API_KEY', value.CARTESIA_API_KEY],
        ['CARTESIA_VOICE_ID', value.CARTESIA_VOICE_ID],
        ['OPENAI_API_KEY', value.OPENAI_API_KEY],
      ],
    } as const satisfies Record<RealtimeProvider, readonly (readonly [string, string | undefined])[]>;

    for (const [name, provided] of byProvider[value.VOICE_PROVIDER]) {
      if (!provided) {
        ctx.addIssue({
          code: 'custom',
          path: [name],
          message: `${name} is required when VOICE_PROVIDER is ${value.VOICE_PROVIDER}`,
        });
      }
    }
  });

interface BaseConfig {
  nodeEnv: 'development' | 'test' | 'production';
  host: string;
  port: number;
  logLevel: 'fatal' | 'error' | 'warn' | 'info' | 'debug' | 'trace' | 'silent';
  databaseUrl: string;
  twilioAuthToken: string;
  twilioAccountSid: string;
  publicBaseUrl: string;
}

export interface OpenAiVoiceConfig {
  voiceProvider: 'openai';
  openaiApiKey: string;
  openaiRealtimeUrl: string;
  openaiTranscribeModel: string;
}

export interface ElevenLabsVoiceConfig {
  voiceProvider: 'elevenlabs';
  elevenLabsApiKey: string;
  elevenLabsAgentId: string;
  elevenLabsApiBaseUrl: string;
}

export interface CartesiaVoiceConfig {
  voiceProvider: 'cartesia';
  cartesiaApiKey: string;
  cartesiaVoiceId: string;
  cartesiaTtsModel: string;
  cartesiaSttModel: string;
  cartesiaVersion: string;
  cartesiaWsBaseUrl: string;
  /** Reasoning turn; Cartesia covers speech only. */
  textLlmApiKey: string;
  textLlmModel: string;
  textLlmBaseUrl: string;
}

/**
 * Discriminated on `voiceProvider`, so the media layer cannot read a credential that
 * the selected provider was never required to supply.
 */
export type AppConfig = BaseConfig & (OpenAiVoiceConfig | ElevenLabsVoiceConfig | CartesiaVoiceConfig);

export function loadConfig(environment: NodeJS.ProcessEnv = process.env): AppConfig {
  const parsed = configSchema.parse(environment);

  const base: BaseConfig = {
    nodeEnv: parsed.NODE_ENV,
    host: parsed.HOST,
    port: parsed.PORT,
    logLevel: parsed.LOG_LEVEL,
    databaseUrl: parsed.DATABASE_URL,
    twilioAuthToken: parsed.TWILIO_AUTH_TOKEN,
    twilioAccountSid: parsed.TWILIO_ACCOUNT_SID,
    publicBaseUrl: parsed.PUBLIC_BASE_URL,
  };

  if (parsed.VOICE_PROVIDER === 'cartesia') {
    return {
      ...base,
      voiceProvider: 'cartesia',
      // Non-null: superRefine already rejected a missing value for this provider.
      cartesiaApiKey: parsed.CARTESIA_API_KEY!,
      cartesiaVoiceId: parsed.CARTESIA_VOICE_ID!,
      cartesiaTtsModel: parsed.CARTESIA_TTS_MODEL,
      cartesiaSttModel: parsed.CARTESIA_STT_MODEL,
      cartesiaVersion: parsed.CARTESIA_VERSION,
      cartesiaWsBaseUrl: parsed.CARTESIA_WS_BASE_URL.replace(/\/$/, ''),
      textLlmApiKey: parsed.OPENAI_API_KEY!,
      textLlmModel: parsed.TEXT_LLM_MODEL,
      textLlmBaseUrl: parsed.TEXT_LLM_BASE_URL.replace(/\/$/, ''),
    };
  }

  if (parsed.VOICE_PROVIDER === 'elevenlabs') {
    return {
      ...base,
      voiceProvider: 'elevenlabs',
      // Non-null: superRefine already rejected a missing value for this provider.
      elevenLabsApiKey: parsed.ELEVENLABS_API_KEY!,
      elevenLabsAgentId: parsed.ELEVENLABS_AGENT_ID!,
      elevenLabsApiBaseUrl: parsed.ELEVENLABS_API_BASE_URL.replace(/\/$/, ''),
    };
  }

  return {
    ...base,
    voiceProvider: 'openai',
    openaiApiKey: parsed.OPENAI_API_KEY!,
    openaiRealtimeUrl: parsed.OPENAI_REALTIME_URL,
    openaiTranscribeModel: parsed.OPENAI_TRANSCRIBE_MODEL,
  };
}

export type { RealtimeProvider };

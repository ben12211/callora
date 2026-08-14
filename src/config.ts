import 'dotenv/config';
import { z } from 'zod';
import { DEFAULT_TRANSCRIPTION_MODEL } from './realtime/protocol.js';
import { DEFAULT_REALTIME_PROVIDER, REALTIME_PROVIDERS, type RealtimeProvider } from './realtime/provider.js';

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
    VOICE_PROVIDER: z.enum([...REALTIME_PROVIDERS]).default(DEFAULT_REALTIME_PROVIDER),
    OPENAI_API_KEY: z.string().min(1).optional(),
    OPENAI_REALTIME_URL: z.string().min(1).default('wss://api.openai.com/v1/realtime'),
    // Transcribes caller audio for conversation logging only; it never drives the reply.
    OPENAI_TRANSCRIBE_MODEL: z.string().min(1).default(DEFAULT_TRANSCRIPTION_MODEL),
    ELEVENLABS_API_KEY: z.string().min(1).optional(),
    ELEVENLABS_AGENT_ID: z.string().min(1).optional(),
    ELEVENLABS_API_BASE_URL: z.string().min(1).default('https://api.elevenlabs.io'),
  })
  .superRefine((value, ctx) => {
    // A deployment only needs credentials for the provider it actually uses, so an
    // OpenAI-only server never has to invent an ElevenLabs key and vice versa.
    const required =
      value.VOICE_PROVIDER === 'openai'
        ? ([['OPENAI_API_KEY', value.OPENAI_API_KEY]] as const)
        : ([
            ['ELEVENLABS_API_KEY', value.ELEVENLABS_API_KEY],
            ['ELEVENLABS_AGENT_ID', value.ELEVENLABS_AGENT_ID],
          ] as const);

    for (const [name, provided] of required) {
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

/**
 * Discriminated on `voiceProvider`, so the media layer cannot read a credential that
 * the selected provider was never required to supply.
 */
export type AppConfig = BaseConfig & (OpenAiVoiceConfig | ElevenLabsVoiceConfig);

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

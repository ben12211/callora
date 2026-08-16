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
    // Control plane. The bootstrap administrator is created (and its password reset) on
    // every start when both values are present, so a fresh stack has a way in.
    ADMIN_EMAIL: blankAsAbsent(z.email().optional()),
    ADMIN_PASSWORD: blankAsAbsent(z.string().min(8).optional()),
    ADMIN_NAME: blankAsAbsent(z.string().min(1).default('Callora Administrator')),
    /** Optional machine credential for the management API; the dashboard never uses it. */
    ADMIN_API_KEY: blankAsAbsent(z.string().min(16).optional()),
    SESSION_TTL_HOURS: z.coerce.number().int().min(1).max(720).default(12),
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

export interface AdminAuthConfig {
  /** Bootstrap administrator, applied at startup when both values are supplied. */
  bootstrapEmail?: string;
  bootstrapPassword?: string;
  bootstrapName: string;
  /** Machine credential accepted by the management API in place of a session cookie. */
  apiKey?: string;
  sessionTtlHours: number;
}

interface BaseConfig {
  nodeEnv: 'development' | 'test' | 'production';
  host: string;
  port: number;
  logLevel: 'fatal' | 'error' | 'warn' | 'info' | 'debug' | 'trace' | 'silent';
  databaseUrl: string;
  twilioAuthToken: string;
  twilioAccountSid: string;
  publicBaseUrl: string;
  auth: AdminAuthConfig;
  /**
   * Credentials for every provider this deployment can execute, independent of which one
   * a given business picked. A `null` entry means the platform has no credentials for
   * that provider, which the provider status page reports and the call path respects.
   */
  providers: ProviderCredentials;
}

/** Platform credentials for OpenAI Realtime. Per-call voice and model come from the agent. */
export interface OpenAiProviderCredentials {
  apiKey: string;
  realtimeUrl: string;
  transcribeModel: string;
}

export interface ElevenLabsProviderCredentials {
  apiKey: string;
  agentId: string;
  apiBaseUrl: string;
}

export interface CartesiaProviderCredentials {
  apiKey: string;
  /** Fallback voice id for agents that did not choose one. */
  defaultVoiceId?: string;
  ttsModel: string;
  sttModel: string;
  version: string;
  wsBaseUrl: string;
  /** Reasoning turn; Cartesia covers speech only. */
  textLlmApiKey: string;
  textLlmModel: string;
  textLlmBaseUrl: string;
}

export interface ProviderCredentials {
  openai: OpenAiProviderCredentials | null;
  elevenlabs: ElevenLabsProviderCredentials | null;
  cartesia: CartesiaProviderCredentials | null;
}

/**
 * One shape for every deployment.
 *
 * `voiceProvider` is only the default for newly created agents: each business stores its
 * own provider, and the call path reads the credentials for that provider out of
 * `providers`. Credentials live in exactly one place, so there is no second copy to keep
 * in step. The startup check in the schema still requires the default provider to be
 * usable, so a deployment cannot come up unable to serve the provider it hands out.
 */
export type AppConfig = BaseConfig & { voiceProvider: RealtimeProvider };

export function loadConfig(environment: NodeJS.ProcessEnv = process.env): AppConfig {
  const parsed = configSchema.parse(environment);

  // A provider is "configured" once its own credentials are present, regardless of which
  // provider VOICE_PROVIDER names. Businesses pick per agent, so several can be live at
  // once and the status page can show exactly which ones are usable.
  const providers: ProviderCredentials = {
    openai: parsed.OPENAI_API_KEY
      ? {
          apiKey: parsed.OPENAI_API_KEY,
          realtimeUrl: parsed.OPENAI_REALTIME_URL,
          transcribeModel: parsed.OPENAI_TRANSCRIBE_MODEL,
        }
      : null,
    elevenlabs:
      parsed.ELEVENLABS_API_KEY && parsed.ELEVENLABS_AGENT_ID
        ? {
            apiKey: parsed.ELEVENLABS_API_KEY,
            agentId: parsed.ELEVENLABS_AGENT_ID,
            apiBaseUrl: parsed.ELEVENLABS_API_BASE_URL.replace(/\/$/, ''),
          }
        : null,
    // Cartesia supplies speech only, so it is unusable without the text LLM key as well.
    cartesia:
      parsed.CARTESIA_API_KEY && parsed.OPENAI_API_KEY
        ? {
            apiKey: parsed.CARTESIA_API_KEY,
            ...(parsed.CARTESIA_VOICE_ID ? { defaultVoiceId: parsed.CARTESIA_VOICE_ID } : {}),
            ttsModel: parsed.CARTESIA_TTS_MODEL,
            sttModel: parsed.CARTESIA_STT_MODEL,
            version: parsed.CARTESIA_VERSION,
            wsBaseUrl: parsed.CARTESIA_WS_BASE_URL.replace(/\/$/, ''),
            textLlmApiKey: parsed.OPENAI_API_KEY,
            textLlmModel: parsed.TEXT_LLM_MODEL,
            textLlmBaseUrl: parsed.TEXT_LLM_BASE_URL.replace(/\/$/, ''),
          }
        : null,
  };

  const base: BaseConfig = {
    providers,
    auth: {
      ...(parsed.ADMIN_EMAIL ? { bootstrapEmail: parsed.ADMIN_EMAIL.toLowerCase() } : {}),
      ...(parsed.ADMIN_PASSWORD ? { bootstrapPassword: parsed.ADMIN_PASSWORD } : {}),
      bootstrapName: parsed.ADMIN_NAME,
      ...(parsed.ADMIN_API_KEY ? { apiKey: parsed.ADMIN_API_KEY } : {}),
      sessionTtlHours: parsed.SESSION_TTL_HOURS,
    },
    nodeEnv: parsed.NODE_ENV,
    host: parsed.HOST,
    port: parsed.PORT,
    logLevel: parsed.LOG_LEVEL,
    databaseUrl: parsed.DATABASE_URL,
    twilioAuthToken: parsed.TWILIO_AUTH_TOKEN,
    twilioAccountSid: parsed.TWILIO_ACCOUNT_SID,
    publicBaseUrl: parsed.PUBLIC_BASE_URL,
  };

  return { ...base, voiceProvider: parsed.VOICE_PROVIDER };
}

export type { RealtimeProvider };

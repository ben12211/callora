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

/**
 * The voice settings a deployment can change at runtime.
 *
 * They are parsed on their own as well as part of the whole configuration, because the
 * dashboard writes these same keys into `platform_settings` and the running process has
 * to re-derive credentials from the merged values without a restart. Everything else
 * below — ports, database, Twilio — is startup-only and is not managed from the UI.
 */
const providerFields = {
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
} as const;

/**
 * Lenient view of the same fields: defaults are applied and anything missing is simply
 * absent. A half-filled provider must leave the dashboard reporting "not configured", not
 * throw inside a live call, so nothing here requires a credential to be present.
 */
const providerSchema = z.object(providerFields);

type ProviderSettings = z.infer<typeof providerSchema>;

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
    ...providerFields,
    // Control plane. The bootstrap administrator is created (and its password reset) on
    // every start when both values are present, so a fresh stack has a way in.
    ADMIN_EMAIL: blankAsAbsent(z.email().optional()),
    ADMIN_PASSWORD: blankAsAbsent(z.string().min(8).optional()),
    ADMIN_NAME: blankAsAbsent(z.string().min(1).default('Callora Administrator')),
    /** Optional machine credential for the management API; the dashboard never uses it. */
    ADMIN_API_KEY: blankAsAbsent(z.string().min(16).optional()),
    SESSION_TTL_HOURS: z.coerce.number().int().min(1).max(720).default(12),
    /**
     * Encrypts the provider credentials the dashboard stores. Without it the dashboard
     * still manages every non-secret setting; it just refuses to write an API key to the
     * database rather than storing one in the clear.
     */
    SECRETS_KEY: blankAsAbsent(z.string().min(16).optional()),
    // Signs the short-lived Media Stream handshake token. Defaults to the Twilio auth
    // token, which is what it used to be; setting this separates the two so rotating one
    // credential does not silently change the other's meaning.
    STREAM_TOKEN_SECRET: blankAsAbsent(z.string().min(16).optional()),
    // Conversation transcripts are the caller's own words, so they are kept for a bounded
    // time by default rather than forever. 0 disables the sweep and keeps them.
    TRANSCRIPT_RETENTION_DAYS: z.coerce.number().int().min(0).max(3650).default(30),
  });

/**
 * Environment variables each provider needs before it can answer a call. They may come
 * from the environment or from `platform_settings`, so this is reported rather than
 * enforced: see `missingProviderCredentials`.
 */
export const REQUIRED_PROVIDER_SETTINGS = {
  openai: ['OPENAI_API_KEY'],
  elevenlabs: ['ELEVENLABS_API_KEY', 'ELEVENLABS_AGENT_ID'],
  // Cartesia covers speech only; the reasoning turn reuses the server-side OpenAI
  // credentials, so that key is required here too.
  cartesia: ['CARTESIA_API_KEY', 'CARTESIA_VOICE_ID', 'OPENAI_API_KEY'],
} as const satisfies Record<RealtimeProvider, readonly string[]>;

/**
 * What the default provider is still missing, for a startup warning.
 *
 * This used to fail configuration parsing outright. Since credentials can now be entered
 * in the dashboard, a deployment that starts with none is a legitimate first boot: the
 * provider page reports what is missing, and until it is filled in, calls answer with the
 * business's static greeting instead of opening a session that could never connect.
 */
export function missingProviderCredentials(values: NodeJS.ProcessEnv): string[] {
  const parsed = providerSchema.parse(values);
  return REQUIRED_PROVIDER_SETTINGS[parsed.VOICE_PROVIDER].filter(
    (name) => !parsed[name as keyof ProviderSettings],
  );
}

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
  /**
   * Secrets accepted for Media Stream tokens, newest first.
   *
   * A list rather than a value so `STREAM_TOKEN_SECRET` can be introduced or rotated
   * without rejecting the tokens already in flight: a stream token lives five minutes,
   * and a call that started before the change still has to be able to connect.
   */
  streamTokenSecrets: string[];
  /** Days a stored transcript is kept; 0 keeps them indefinitely. */
  transcriptRetentionDays: number;
  twilioAccountSid: string;
  publicBaseUrl: string;
  auth: AdminAuthConfig;
  /**
   * Credentials for every provider this deployment can execute, independent of which one
   * a given business picked. A `null` entry means the platform has no credentials for
   * that provider, which the provider status page reports and the call path respects.
   *
   * This is the value the process started with. Once the dashboard has written provider
   * settings, the live values come from `PlatformSettings`, and the call path reads them
   * from there rather than from here.
   */
  providers: ProviderCredentials;
  /** Encrypts dashboard-managed credentials; absent means the UI manages non-secrets only. */
  secretsKey?: string;
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
 * in step.
 */
export type AppConfig = BaseConfig & { voiceProvider: RealtimeProvider };

/**
 * Derives provider credentials from settings values, wherever they came from: the process
 * environment at startup, or the dashboard-managed overrides merged over it.
 *
 * A provider is "configured" once its own credentials are present, regardless of which
 * provider VOICE_PROVIDER names. Businesses pick per agent, so several can be live at
 * once and the status page can show exactly which ones are usable.
 */
export function resolveProviderSettings(values: NodeJS.ProcessEnv): {
  providers: ProviderCredentials;
  voiceProvider: RealtimeProvider;
} {
  const parsed = providerSchema.parse(values);
  return { providers: buildProviders(parsed), voiceProvider: parsed.VOICE_PROVIDER };
}

function buildProviders(parsed: ProviderSettings): ProviderCredentials {
  return {
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
}

export function loadConfig(environment: NodeJS.ProcessEnv = process.env): AppConfig {
  const parsed = configSchema.parse(environment);

  const base: BaseConfig = {
    providers: buildProviders(parsed),
    ...(parsed.SECRETS_KEY ? { secretsKey: parsed.SECRETS_KEY } : {}),
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
    transcriptRetentionDays: parsed.TRANSCRIPT_RETENTION_DAYS,
    streamTokenSecrets: parsed.STREAM_TOKEN_SECRET
      ? [parsed.STREAM_TOKEN_SECRET, parsed.TWILIO_AUTH_TOKEN]
      : [parsed.TWILIO_AUTH_TOKEN],
    twilioAccountSid: parsed.TWILIO_ACCOUNT_SID,
    publicBaseUrl: parsed.PUBLIC_BASE_URL,
  };

  return { ...base, voiceProvider: parsed.VOICE_PROVIDER };
}

export type { RealtimeProvider };

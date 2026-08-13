import 'dotenv/config';
import { z } from 'zod';
import { DEFAULT_TRANSCRIPTION_MODEL } from './realtime/protocol.js';

const configSchema = z.object({
  NODE_ENV: z.enum(['development', 'test', 'production']).default('development'),
  HOST: z.string().min(1).default('0.0.0.0'),
  PORT: z.coerce.number().int().min(1).max(65535).default(3000),
  LOG_LEVEL: z.enum(['fatal', 'error', 'warn', 'info', 'debug', 'trace', 'silent']).default('info'),
  DATABASE_URL: z.string().min(1),
  TWILIO_AUTH_TOKEN: z.string().min(1),
  // Required: the REST client that hangs calls up is authenticated per account.
  TWILIO_ACCOUNT_SID: z.string().regex(/^AC[0-9a-fA-F]{32}$/, 'TWILIO_ACCOUNT_SID must be a Twilio Account SID'),
  PUBLIC_BASE_URL: z.url().transform((value) => value.replace(/\/$/, '')),
  OPENAI_API_KEY: z.string().min(1),
  OPENAI_REALTIME_URL: z.string().min(1).default('wss://api.openai.com/v1/realtime'),
  // Transcribes caller audio for conversation logging only; it never drives the reply.
  OPENAI_TRANSCRIBE_MODEL: z.string().min(1).default(DEFAULT_TRANSCRIPTION_MODEL),
});

export interface AppConfig {
  nodeEnv: 'development' | 'test' | 'production';
  host: string;
  port: number;
  logLevel: 'fatal' | 'error' | 'warn' | 'info' | 'debug' | 'trace' | 'silent';
  databaseUrl: string;
  twilioAuthToken: string;
  twilioAccountSid: string;
  publicBaseUrl: string;
  openaiApiKey: string;
  openaiRealtimeUrl: string;
  openaiTranscribeModel: string;
}

export function loadConfig(environment: NodeJS.ProcessEnv = process.env): AppConfig {
  const parsed = configSchema.parse(environment);

  return {
    nodeEnv: parsed.NODE_ENV,
    host: parsed.HOST,
    port: parsed.PORT,
    logLevel: parsed.LOG_LEVEL,
    databaseUrl: parsed.DATABASE_URL,
    twilioAuthToken: parsed.TWILIO_AUTH_TOKEN,
    twilioAccountSid: parsed.TWILIO_ACCOUNT_SID,
    publicBaseUrl: parsed.PUBLIC_BASE_URL,
    openaiApiKey: parsed.OPENAI_API_KEY,
    openaiRealtimeUrl: parsed.OPENAI_REALTIME_URL,
    openaiTranscribeModel: parsed.OPENAI_TRANSCRIBE_MODEL,
  };
}

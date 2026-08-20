import type { RealtimeProvider } from '../realtime/provider.js';

export interface Business {
  id: string;
  name: string;
  phoneNumber: string;
  greeting: string;
  active: boolean;
  createdAt: Date;
  updatedAt: Date;
}

export interface CreateBusinessInput {
  name: string;
  phoneNumber: string;
  greeting: string;
  active: boolean;
}

export type UpdateBusinessInput = Partial<CreateBusinessInput>;

/** Per-business AI agent configuration used by the realtime call path. */
export interface AgentConfig {
  businessId: string;
  instructions: string;
  greeting: string;
  language: string;
  voice: string;
  realtimeModel: string;
  /** Execution provider that answers this business's calls. */
  voiceProvider: RealtimeProvider;
  /**
   * ElevenLabs agent this business owns. Empty falls back to the platform-wide agent,
   * which is the pre-existing behaviour; Callora only writes configuration into an agent
   * a business owns, so a shared one is never overwritten.
   */
  elevenLabsAgentId: string;
  enabled: boolean;
  createdAt: Date;
  updatedAt: Date;
}

/** Full agent configuration written from the control plane; every field is replaced. */
export interface UpsertAgentConfigInput {
  instructions: string;
  greeting: string;
  language: string;
  voice: string;
  realtimeModel: string;
  voiceProvider: RealtimeProvider;
  elevenLabsAgentId: string;
  enabled: boolean;
}

/**
 * One dashboard-managed platform setting, keyed by the environment variable it overrides.
 * Secret values are stored sealed and are only ever decrypted in the server process.
 */
export interface PlatformSetting {
  key: string;
  value: string;
  secret: boolean;
  updatedAt: Date;
}

export interface UpsertPlatformSettingInput {
  key: string;
  value: string;
  secret: boolean;
}

/**
 * What an administrator is allowed to reach.
 *
 * `platform` is what every account was before per-tenant authorization existed: access to
 * every business, call, and provider credential. `business` is scoped to exactly one
 * tenant and can never see another's data or the platform's credentials.
 */
export type AdminRole = 'platform' | 'business';

export interface AdminUser {
  id: string;
  email: string;
  name: string;
  passwordHash: string;
  role: AdminRole;
  /** Set for a `business` administrator, and null for a platform one. */
  businessId: string | null;
  active: boolean;
  lastLoginAt: Date | null;
  createdAt: Date;
  updatedAt: Date;
}

export interface CreateAdminUserInput {
  email: string;
  name: string;
  passwordHash: string;
  role?: AdminRole;
  businessId?: string | null;
}

export interface AdminSession {
  id: string;
  adminUserId: string;
  csrfToken: string;
  expiresAt: Date;
  createdAt: Date;
}

export interface CreateAdminSessionInput {
  adminUserId: string;
  tokenHash: string;
  csrfToken: string;
  expiresAt: Date;
}

export interface AuditEvent {
  id: string;
  actorId: string | null;
  actorLabel: string;
  action: string;
  entityType: string;
  entityId: string | null;
  summary: string;
  details: Record<string, unknown>;
  createdAt: Date;
}

export interface RecordAuditEventInput {
  actorId: string | null;
  actorLabel: string;
  action: string;
  entityType: string;
  entityId: string | null;
  summary: string;
  details?: Record<string, unknown>;
}

export interface ListAuditEventsOptions {
  entityType?: string;
  entityId?: string;
  limit: number;
  offset: number;
}

export interface CallRecord {
  id: string;
  businessId: string;
  twilioCallSid: string;
  twilioStreamSid: string | null;
  /** The provider's own session/conversation id, whichever provider ran the call. */
  providerSessionId: string | null;
  /** Which backend ran it: `openai`, `elevenlabs`, or `cartesia`. */
  provider: string | null;
  fromNumber: string | null;
  toNumber: string;
  status: string;
  direction: string | null;
  durationSeconds: number | null;
  startedAt: Date;
  endedAt: Date | null;
  createdAt: Date;
  updatedAt: Date;
}

export interface UpsertCallInput {
  businessId: string;
  twilioCallSid: string;
  fromNumber: string | null;
  toNumber: string;
  status: string;
  direction: string | null;
}

export interface UpdateCallStatusInput {
  businessId: string;
  twilioCallSid: string;
  toNumber: string;
  status: string;
  durationSeconds: number | null;
}

export interface AttachRealtimeSessionInput {
  businessId: string;
  twilioCallSid: string;
  twilioStreamSid: string | null;
  providerSessionId: string | null;
  provider?: string | null;
}

/** One completed turn of a conversation. */
export interface CallTranscriptTurn {
  id: string;
  callId: string;
  businessId: string;
  speaker: 'caller' | 'agent';
  content: string;
  turn: number;
  createdAt: Date;
}

export interface AppendTranscriptInput {
  callId: string;
  businessId: string;
  speaker: 'caller' | 'agent';
  content: string;
  turn: number;
}

export interface ListCallsOptions {
  businessId?: string;
  limit: number;
  offset: number;
}

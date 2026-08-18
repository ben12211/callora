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

export interface AdminUser {
  id: string;
  email: string;
  name: string;
  passwordHash: string;
  active: boolean;
  lastLoginAt: Date | null;
  createdAt: Date;
  updatedAt: Date;
}

export interface CreateAdminUserInput {
  email: string;
  name: string;
  passwordHash: string;
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
  openaiSessionId: string | null;
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
  openaiSessionId: string | null;
}

export interface ListCallsOptions {
  businessId?: string;
  limit: number;
  offset: number;
}

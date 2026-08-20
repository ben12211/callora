import { randomUUID } from 'node:crypto';
import type { AppConfig } from '../../src/config.js';
import type { DataStore } from '../../src/db/store.js';
import type {
  AdminSession,
  AdminUser,
  AppendTranscriptInput,
  CallTranscriptTurn,
  AgentConfig,
  AttachRealtimeSessionInput,
  AuditEvent,
  Business,
  CallRecord,
  CreateAdminSessionInput,
  CreateAdminUserInput,
  CreateBusinessInput,
  ListAuditEventsOptions,
  ListCallsOptions,
  PlatformSetting,
  RecordAuditEventInput,
  UpdateBusinessInput,
  UpdateCallStatusInput,
  UpsertAgentConfigInput,
  UpsertCallInput,
  UpsertPlatformSettingInput,
} from '../../src/domain/models.js';

export const firstBusinessId = '00000000-0000-4000-8000-000000000001';
export const secondBusinessId = '00000000-0000-4000-8000-000000000002';
export const firstNumber = '+15551234567';
export const secondNumber = '+15557654321';

export function business(id: string, name: string, phoneNumber: string, greeting: string): Business {
  const now = new Date();
  return { id, name, phoneNumber, greeting, active: true, createdAt: now, updatedAt: now };
}

export function agentConfig(businessId: string, enabled = true): AgentConfig {
  const now = new Date();
  return {
    businessId,
    instructions: 'Be a concise Hebrew customer-service agent.',
    greeting: 'שלום, איך אפשר לעזור?',
    language: 'he-IL',
    voice: 'marin',
    realtimeModel: 'gpt-realtime-2.1',
    voiceProvider: 'openai',
    elevenLabsAgentId: '',
    enabled,
    createdAt: now,
    updatedAt: now,
  };
}

/** In-memory `DataStore`, so the HTTP tests never need a PostgreSQL instance. */
export class MemoryStore implements DataStore {
  public agentConfigs: AgentConfig[] = [];

  public businesses: Business[] = [
    business(firstBusinessId, 'First Business', firstNumber, 'Hello from the first business.'),
    business(secondBusinessId, 'Second Business', secondNumber, 'Hello from the second business.'),
  ];

  public calls: CallRecord[] = [];
  public platformSettings: PlatformSetting[] = [];
  public healthy = true;
  public admins: AdminUser[] = [];
  public auditEvents: AuditEvent[] = [];

  private sessions = new Map<string, AdminSession>();

  public async ping(): Promise<void> {
    if (!this.healthy) {
      throw new Error('database unavailable');
    }
  }

  public async listBusinesses(): Promise<Business[]> {
    return this.businesses;
  }

  // Reads hand back copies, the way a real query would. Callers that compare a record
  // before and after an update must not be looking at the same object twice.
  public async getBusinessById(id: string): Promise<Business | null> {
    const found = this.businesses.find((item) => item.id === id);
    return found ? { ...found } : null;
  }

  public async getBusinessByPhoneNumber(phoneNumber: string, activeOnly = true): Promise<Business | null> {
    const found = this.businesses.find(
      (item) => item.phoneNumber === phoneNumber && (!activeOnly || item.active),
    );
    return found ? { ...found } : null;
  }

  public async createBusiness(input: CreateBusinessInput): Promise<Business> {
    const created = business(randomUUID(), input.name, input.phoneNumber, input.greeting);
    created.active = input.active;
    this.businesses.push(created);
    return created;
  }

  public async updateBusiness(id: string, input: UpdateBusinessInput): Promise<Business | null> {
    const existing = this.businesses.find((item) => item.id === id);
    if (!existing) {
      return null;
    }
    Object.assign(existing, input, { updatedAt: new Date() });
    return { ...existing };
  }

  public async deleteBusiness(id: string): Promise<Business | null> {
    if (this.calls.some((call) => call.businessId === id)) {
      return null;
    }
    const index = this.businesses.findIndex((item) => item.id === id);
    if (index === -1) {
      return null;
    }
    return this.businesses.splice(index, 1)[0] ?? null;
  }

  public async getAgentConfig(businessId: string): Promise<AgentConfig | null> {
    const found = this.agentConfigs.find((item) => item.businessId === businessId);
    return found ? { ...found } : null;
  }

  public async upsertAgentConfig(businessId: string, input: UpsertAgentConfigInput): Promise<AgentConfig> {
    const existing = this.agentConfigs.find((item) => item.businessId === businessId);
    if (existing) {
      Object.assign(existing, input, { updatedAt: new Date() });
      return { ...existing };
    }
    const now = new Date();
    const created: AgentConfig = { businessId, ...input, createdAt: now, updatedAt: now };
    this.agentConfigs.push(created);
    return created;
  }

  public async attachRealtimeSession(input: AttachRealtimeSessionInput): Promise<CallRecord | null> {
    const existing = this.calls.find(
      (call) => call.twilioCallSid === input.twilioCallSid && call.businessId === input.businessId,
    );
    if (!existing) {
      return null;
    }
    existing.twilioStreamSid = input.twilioStreamSid ?? existing.twilioStreamSid;
    existing.providerSessionId = input.providerSessionId ?? existing.providerSessionId;
    existing.provider = input.provider ?? existing.provider;
    return existing;
  }

  public async getCallByTwilioSid(twilioCallSid: string): Promise<CallRecord | null> {
    return this.calls.find((call) => call.twilioCallSid === twilioCallSid) ?? null;
  }

  public async upsertCall(input: UpsertCallInput): Promise<CallRecord> {
    const existing = this.calls.find((call) => call.twilioCallSid === input.twilioCallSid);
    if (existing) {
      existing.status = input.status;
      existing.updatedAt = new Date();
      return existing;
    }
    const now = new Date();
    const created: CallRecord = {
      id: randomUUID(),
      ...input,
      twilioStreamSid: null,
      providerSessionId: null,
      provider: null,
      durationSeconds: null,
      startedAt: now,
      endedAt: null,
      createdAt: now,
      updatedAt: now,
    };
    this.calls.push(created);
    return created;
  }

  public async updateCallStatus(input: UpdateCallStatusInput): Promise<CallRecord | null> {
    const existing = this.calls.find(
      (call) =>
        call.twilioCallSid === input.twilioCallSid &&
        call.businessId === input.businessId &&
        call.toNumber === input.toNumber,
    );
    if (!existing) {
      return null;
    }
    existing.status = input.status;
    existing.durationSeconds = input.durationSeconds;
    existing.updatedAt = new Date();
    return existing;
  }

  public async listCalls(options: ListCallsOptions): Promise<CallRecord[]> {
    return this.calls
      .filter((call) => !options.businessId || call.businessId === options.businessId)
      .slice(options.offset, options.offset + options.limit);
  }

  public async getCallById(id: string): Promise<CallRecord | null> {
    return this.calls.find((call) => call.id === id) ?? null;
  }

  public async countCalls(businessId?: string): Promise<number> {
    return this.calls.filter((call) => !businessId || call.businessId === businessId).length;
  }

  public transcripts: CallTranscriptTurn[] = [];

  public async appendTranscriptTurn(input: AppendTranscriptInput): Promise<CallTranscriptTurn> {
    const existing = this.transcripts.find(
      (turn) => turn.callId === input.callId && turn.turn === input.turn,
    );
    if (existing) {
      existing.content = input.content;
      return existing;
    }
    const stored: CallTranscriptTurn = {
      id: `transcript-${this.transcripts.length + 1}`,
      callId: input.callId,
      businessId: input.businessId,
      speaker: input.speaker,
      content: input.content,
      turn: input.turn,
      createdAt: new Date(),
    };
    this.transcripts.push(stored);
    return stored;
  }

  public async listTranscript(callId: string): Promise<CallTranscriptTurn[]> {
    return this.transcripts
      .filter((turn) => turn.callId === callId)
      .sort((left, right) => left.turn - right.turn)
      .map((turn) => ({ ...turn }));
  }

  public async deleteTranscriptsOlderThan(cutoff: Date): Promise<number> {
    const before = this.transcripts.length;
    this.transcripts = this.transcripts.filter((turn) => turn.createdAt >= cutoff);
    return before - this.transcripts.length;
  }

  public async listPlatformSettings(): Promise<PlatformSetting[]> {
    return this.platformSettings.map((setting) => ({ ...setting }));
  }

  public async upsertPlatformSetting(input: UpsertPlatformSettingInput): Promise<PlatformSetting> {
    const stored: PlatformSetting = { ...input, updatedAt: new Date() };
    const index = this.platformSettings.findIndex((setting) => setting.key === input.key);
    if (index === -1) {
      this.platformSettings.push(stored);
    } else {
      this.platformSettings[index] = stored;
    }
    return { ...stored };
  }

  public async deletePlatformSetting(key: string): Promise<void> {
    this.platformSettings = this.platformSettings.filter((setting) => setting.key !== key);
  }

  public async listAdminUsers(): Promise<AdminUser[]> {
    return this.admins;
  }

  public async getAdminUserByEmail(email: string): Promise<AdminUser | null> {
    return this.admins.find((admin) => admin.email === email.toLowerCase()) ?? null;
  }

  public async getAdminUserById(id: string): Promise<AdminUser | null> {
    return this.admins.find((admin) => admin.id === id) ?? null;
  }

  public async createAdminUser(input: CreateAdminUserInput): Promise<AdminUser> {
    const now = new Date();
    const created: AdminUser = {
      id: randomUUID(),
      email: input.email.toLowerCase(),
      name: input.name,
      passwordHash: input.passwordHash,
      active: true,
      lastLoginAt: null,
      createdAt: now,
      updatedAt: now,
    };
    this.admins.push(created);
    return created;
  }

  public async setAdminUserPassword(id: string, passwordHash: string): Promise<AdminUser | null> {
    const admin = await this.getAdminUserById(id);
    if (!admin) {
      return null;
    }
    admin.passwordHash = passwordHash;
    admin.updatedAt = new Date();
    return admin;
  }

  public async recordAdminLogin(id: string): Promise<void> {
    const admin = await this.getAdminUserById(id);
    if (admin) {
      admin.lastLoginAt = new Date();
    }
  }

  public async createAdminSession(input: CreateAdminSessionInput): Promise<AdminSession> {
    const session: AdminSession = {
      id: randomUUID(),
      adminUserId: input.adminUserId,
      csrfToken: input.csrfToken,
      expiresAt: input.expiresAt,
      createdAt: new Date(),
    };
    this.sessions.set(input.tokenHash, session);
    return session;
  }

  public async findAdminSession(tokenHash: string): Promise<{ session: AdminSession; user: AdminUser } | null> {
    const session = this.sessions.get(tokenHash);
    if (!session || session.expiresAt.getTime() <= Date.now()) {
      return null;
    }
    const user = await this.getAdminUserById(session.adminUserId);
    return user && user.active ? { session, user } : null;
  }

  public async deleteAdminSession(tokenHash: string): Promise<void> {
    this.sessions.delete(tokenHash);
  }

  public async deleteAdminSessionsForUser(adminUserId: string): Promise<void> {
    for (const [hash, session] of this.sessions) {
      if (session.adminUserId === adminUserId) {
        this.sessions.delete(hash);
      }
    }
  }

  public async deleteExpiredAdminSessions(): Promise<void> {
    for (const [hash, session] of this.sessions) {
      if (session.expiresAt.getTime() <= Date.now()) {
        this.sessions.delete(hash);
      }
    }
  }

  public async recordAuditEvent(input: RecordAuditEventInput): Promise<AuditEvent> {
    const event: AuditEvent = {
      id: randomUUID(),
      actorId: input.actorId,
      actorLabel: input.actorLabel,
      action: input.action,
      entityType: input.entityType,
      entityId: input.entityId,
      summary: input.summary,
      details: input.details ?? {},
      createdAt: new Date(),
    };
    this.auditEvents.unshift(event);
    return event;
  }

  public async listAuditEvents(options: ListAuditEventsOptions): Promise<AuditEvent[]> {
    return this.auditEvents
      .filter(
        (event) =>
          (!options.entityType || event.entityType === options.entityType) &&
          (!options.entityId || event.entityId === options.entityId),
      )
      .slice(options.offset, options.offset + options.limit);
  }
}

/** Base test configuration: OpenAI is the platform default and the only configured provider. */
export const testConfig: AppConfig = {
  nodeEnv: 'test',
  host: '127.0.0.1',
  port: 3000,
  logLevel: 'silent',
  databaseUrl: 'postgresql://unused',
  twilioAccountSid: 'AC00000000000000000000000000000000',
  twilioAuthToken: 'test-auth-token',
  transcriptRetentionDays: 30,
  // Matches the default: without STREAM_TOKEN_SECRET the Twilio auth token still signs
  // media-stream tokens, so the existing tests keep minting and verifying with it.
  streamTokenSecrets: ['test-auth-token'],
  publicBaseUrl: 'https://voice.example.test',
  auth: { bootstrapName: 'Callora Administrator', sessionTtlHours: 12 },
  providers: {
    openai: {
      apiKey: 'test-openai-key',
      realtimeUrl: 'wss://api.openai.com/v1/realtime',
      transcribeModel: 'gpt-4o-mini-transcribe',
    },
    elevenlabs: null,
    cartesia: null,
  },
  voiceProvider: 'openai',
};

import { randomUUID } from 'node:crypto';
import type pg from 'pg';
import type {
  AdminSession,
  AdminUser,
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
} from '../domain/models.js';
import type { RealtimeProvider } from '../realtime/provider.js';
import type { DataStore } from './store.js';

interface BusinessRow {
  id: string;
  name: string;
  phone_number: string;
  greeting: string;
  active: boolean;
  created_at: Date;
  updated_at: Date;
}

interface AgentConfigRow {
  business_id: string;
  instructions: string;
  greeting: string;
  language: string;
  voice: string;
  realtime_model: string;
  voice_provider: RealtimeProvider;
  enabled: boolean;
  created_at: Date;
  updated_at: Date;
}

interface AdminUserRow {
  id: string;
  email: string;
  name: string;
  password_hash: string;
  active: boolean;
  last_login_at: Date | null;
  created_at: Date;
  updated_at: Date;
}

interface AdminSessionRow {
  id: string;
  admin_user_id: string;
  csrf_token: string;
  expires_at: Date;
  created_at: Date;
}

interface AuditEventRow {
  id: string;
  actor_id: string | null;
  actor_label: string;
  action: string;
  entity_type: string;
  entity_id: string | null;
  summary: string;
  details: Record<string, unknown> | null;
  created_at: Date;
}

interface PlatformSettingRow {
  key: string;
  value: string;
  secret: boolean;
  updated_at: Date;
}

interface CallRow {
  id: string;
  business_id: string;
  twilio_call_sid: string;
  twilio_stream_sid: string | null;
  openai_session_id: string | null;
  from_number: string | null;
  to_number: string;
  status: string;
  direction: string | null;
  duration_seconds: number | null;
  started_at: Date;
  ended_at: Date | null;
  created_at: Date;
  updated_at: Date;
}

function mapBusiness(row: BusinessRow): Business {
  return {
    id: row.id,
    name: row.name,
    phoneNumber: row.phone_number,
    greeting: row.greeting,
    active: row.active,
    createdAt: row.created_at,
    updatedAt: row.updated_at,
  };
}

function mapCall(row: CallRow): CallRecord {
  return {
    id: row.id,
    businessId: row.business_id,
    twilioCallSid: row.twilio_call_sid,
    twilioStreamSid: row.twilio_stream_sid,
    openaiSessionId: row.openai_session_id,
    fromNumber: row.from_number,
    toNumber: row.to_number,
    status: row.status,
    direction: row.direction,
    durationSeconds: row.duration_seconds,
    startedAt: row.started_at,
    endedAt: row.ended_at,
    createdAt: row.created_at,
    updatedAt: row.updated_at,
  };
}

const businessColumns = `
  id, name, phone_number, greeting, active, created_at, updated_at
`;

function mapAgentConfig(row: AgentConfigRow): AgentConfig {
  return {
    businessId: row.business_id,
    instructions: row.instructions,
    greeting: row.greeting,
    language: row.language,
    voice: row.voice,
    realtimeModel: row.realtime_model,
    voiceProvider: row.voice_provider,
    enabled: row.enabled,
    createdAt: row.created_at,
    updatedAt: row.updated_at,
  };
}

const agentConfigColumns = `
  business_id, instructions, greeting, language, voice, realtime_model, voice_provider,
  enabled, created_at, updated_at
`;

function mapAdminUser(row: AdminUserRow): AdminUser {
  return {
    id: row.id,
    email: row.email,
    name: row.name,
    passwordHash: row.password_hash,
    active: row.active,
    lastLoginAt: row.last_login_at,
    createdAt: row.created_at,
    updatedAt: row.updated_at,
  };
}

const adminUserColumns = `
  id, email, name, password_hash, active, last_login_at, created_at, updated_at
`;

function mapAdminSession(row: AdminSessionRow): AdminSession {
  return {
    id: row.id,
    adminUserId: row.admin_user_id,
    csrfToken: row.csrf_token,
    expiresAt: row.expires_at,
    createdAt: row.created_at,
  };
}

function mapAuditEvent(row: AuditEventRow): AuditEvent {
  return {
    id: row.id,
    actorId: row.actor_id,
    actorLabel: row.actor_label,
    action: row.action,
    entityType: row.entity_type,
    entityId: row.entity_id,
    summary: row.summary,
    details: row.details ?? {},
    createdAt: row.created_at,
  };
}

const auditEventColumns = `
  id, actor_id, actor_label, action, entity_type, entity_id, summary, details, created_at
`;

const callColumns = `
  id, business_id, twilio_call_sid, twilio_stream_sid, openai_session_id,
  from_number, to_number, status, direction,
  duration_seconds, started_at, ended_at, created_at, updated_at
`;

function mapPlatformSetting(row: PlatformSettingRow): PlatformSetting {
  return { key: row.key, value: row.value, secret: row.secret, updatedAt: row.updated_at };
}

const platformSettingColumns = `key, value, secret, updated_at`;

export class PostgresStore implements DataStore {
  public constructor(private readonly pool: pg.Pool) {}

  public async ping(): Promise<void> {
    await this.pool.query('SELECT 1');
  }

  public async listBusinesses(): Promise<Business[]> {
    const result = await this.pool.query<BusinessRow>(
      `SELECT ${businessColumns} FROM businesses ORDER BY created_at ASC`,
    );
    return result.rows.map(mapBusiness);
  }

  public async getBusinessById(id: string): Promise<Business | null> {
    const result = await this.pool.query<BusinessRow>(
      `SELECT ${businessColumns} FROM businesses WHERE id = $1`,
      [id],
    );
    return result.rows[0] ? mapBusiness(result.rows[0]) : null;
  }

  public async getBusinessByPhoneNumber(phoneNumber: string, activeOnly = true): Promise<Business | null> {
    const result = await this.pool.query<BusinessRow>(
      `SELECT ${businessColumns}
       FROM businesses
       WHERE phone_number = $1 AND ($2::boolean = false OR active = true)`,
      [phoneNumber, activeOnly],
    );
    return result.rows[0] ? mapBusiness(result.rows[0]) : null;
  }

  public async createBusiness(input: CreateBusinessInput): Promise<Business> {
    const result = await this.pool.query<BusinessRow>(
      `INSERT INTO businesses (id, name, phone_number, greeting, active)
       VALUES ($1, $2, $3, $4, $5)
       RETURNING ${businessColumns}`,
      [randomUUID(), input.name, input.phoneNumber, input.greeting, input.active],
    );
    return mapBusiness(result.rows[0]!);
  }

  public async updateBusiness(id: string, input: UpdateBusinessInput): Promise<Business | null> {
    const updates: string[] = [];
    const values: unknown[] = [];

    for (const [column, value] of [
      ['name', input.name],
      ['phone_number', input.phoneNumber],
      ['greeting', input.greeting],
      ['active', input.active],
    ] as const) {
      if (value !== undefined) {
        values.push(value);
        updates.push(`${column} = $${values.length}`);
      }
    }

    if (updates.length === 0) {
      return this.getBusinessById(id);
    }

    values.push(id);
    const result = await this.pool.query<BusinessRow>(
      `UPDATE businesses
       SET ${updates.join(', ')}, updated_at = now()
       WHERE id = $${values.length}
       RETURNING ${businessColumns}`,
      values,
    );
    return result.rows[0] ? mapBusiness(result.rows[0]) : null;
  }

  public async deleteBusiness(id: string): Promise<Business | null> {
    const result = await this.pool.query<BusinessRow>(
      `DELETE FROM businesses
       WHERE id = $1
         AND NOT EXISTS (SELECT 1 FROM calls WHERE business_id = $1)
       RETURNING ${businessColumns}`,
      [id],
    );
    return result.rows[0] ? mapBusiness(result.rows[0]) : null;
  }

  public async getAgentConfig(businessId: string): Promise<AgentConfig | null> {
    const result = await this.pool.query<AgentConfigRow>(
      `SELECT ${agentConfigColumns} FROM agent_configs WHERE business_id = $1`,
      [businessId],
    );
    return result.rows[0] ? mapAgentConfig(result.rows[0]) : null;
  }

  public async upsertAgentConfig(businessId: string, input: UpsertAgentConfigInput): Promise<AgentConfig> {
    const result = await this.pool.query<AgentConfigRow>(
      `INSERT INTO agent_configs (
         business_id, instructions, greeting, language, voice, realtime_model, voice_provider, enabled
       ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
       ON CONFLICT (business_id) DO UPDATE SET
         instructions = EXCLUDED.instructions,
         greeting = EXCLUDED.greeting,
         language = EXCLUDED.language,
         voice = EXCLUDED.voice,
         realtime_model = EXCLUDED.realtime_model,
         voice_provider = EXCLUDED.voice_provider,
         enabled = EXCLUDED.enabled,
         updated_at = now()
       RETURNING ${agentConfigColumns}`,
      [
        businessId,
        input.instructions,
        input.greeting,
        input.language,
        input.voice,
        input.realtimeModel,
        input.voiceProvider,
        input.enabled,
      ],
    );
    return mapAgentConfig(result.rows[0]!);
  }

  public async attachRealtimeSession(input: AttachRealtimeSessionInput): Promise<CallRecord | null> {
    const result = await this.pool.query<CallRow>(
      `UPDATE calls
       SET twilio_stream_sid = COALESCE($1, twilio_stream_sid),
           openai_session_id = COALESCE($2, openai_session_id),
           updated_at = now()
       WHERE twilio_call_sid = $3 AND business_id = $4
       RETURNING ${callColumns}`,
      [input.twilioStreamSid, input.openaiSessionId, input.twilioCallSid, input.businessId],
    );
    return result.rows[0] ? mapCall(result.rows[0]) : null;
  }

  public async upsertCall(input: UpsertCallInput): Promise<CallRecord> {
    const result = await this.pool.query<CallRow>(
      `INSERT INTO calls (
         id, business_id, twilio_call_sid, from_number, to_number, status, direction
       ) VALUES ($1, $2, $3, $4, $5, $6, $7)
       ON CONFLICT (twilio_call_sid) DO UPDATE SET
         status = EXCLUDED.status,
         direction = COALESCE(EXCLUDED.direction, calls.direction),
         updated_at = now()
       RETURNING ${callColumns}`,
      [
        randomUUID(),
        input.businessId,
        input.twilioCallSid,
        input.fromNumber,
        input.toNumber,
        input.status,
        input.direction,
      ],
    );
    return mapCall(result.rows[0]!);
  }

  public async updateCallStatus(input: UpdateCallStatusInput): Promise<CallRecord | null> {
    const terminalStatuses = ['completed', 'busy', 'failed', 'no-answer', 'canceled'];
    const result = await this.pool.query<CallRow>(
      `UPDATE calls
       SET status = $1,
           duration_seconds = COALESCE($2, duration_seconds),
           ended_at = CASE WHEN $1 = ANY($3::text[]) THEN COALESCE(ended_at, now()) ELSE ended_at END,
           updated_at = now()
       WHERE twilio_call_sid = $4 AND business_id = $5 AND to_number = $6
       RETURNING ${callColumns}`,
      [
        input.status,
        input.durationSeconds,
        terminalStatuses,
        input.twilioCallSid,
        input.businessId,
        input.toNumber,
      ],
    );
    return result.rows[0] ? mapCall(result.rows[0]) : null;
  }

  public async listCalls(options: ListCallsOptions): Promise<CallRecord[]> {
    const values: unknown[] = [];
    let where = '';
    if (options.businessId) {
      values.push(options.businessId);
      where = `WHERE business_id = $${values.length}`;
    }
    values.push(options.limit, options.offset);
    const result = await this.pool.query<CallRow>(
      `SELECT ${callColumns}
       FROM calls
       ${where}
       ORDER BY created_at DESC
       LIMIT $${values.length - 1} OFFSET $${values.length}`,
      values,
    );
    return result.rows.map(mapCall);
  }

  public async getCallById(id: string): Promise<CallRecord | null> {
    const result = await this.pool.query<CallRow>(
      `SELECT ${callColumns} FROM calls WHERE id = $1`,
      [id],
    );
    return result.rows[0] ? mapCall(result.rows[0]) : null;
  }

  public async getCallByTwilioSid(twilioCallSid: string): Promise<CallRecord | null> {
    const result = await this.pool.query<CallRow>(
      `SELECT ${callColumns} FROM calls WHERE twilio_call_sid = $1`,
      [twilioCallSid],
    );
    return result.rows[0] ? mapCall(result.rows[0]) : null;
  }

  public async countCalls(businessId?: string): Promise<number> {
    const result = await this.pool.query<{ count: string }>(
      `SELECT count(*)::text AS count FROM calls WHERE ($1::uuid IS NULL OR business_id = $1)`,
      [businessId ?? null],
    );
    return Number(result.rows[0]?.count ?? 0);
  }

  public async listPlatformSettings(): Promise<PlatformSetting[]> {
    const result = await this.pool.query<PlatformSettingRow>(
      `SELECT ${platformSettingColumns} FROM platform_settings ORDER BY key`,
    );
    return result.rows.map(mapPlatformSetting);
  }

  public async upsertPlatformSetting(input: UpsertPlatformSettingInput): Promise<PlatformSetting> {
    const result = await this.pool.query<PlatformSettingRow>(
      `INSERT INTO platform_settings (key, value, secret)
       VALUES ($1, $2, $3)
       ON CONFLICT (key) DO UPDATE SET
         value = EXCLUDED.value,
         secret = EXCLUDED.secret,
         updated_at = now()
       RETURNING ${platformSettingColumns}`,
      [input.key, input.value, input.secret],
    );
    return mapPlatformSetting(result.rows[0]!);
  }

  public async deletePlatformSetting(key: string): Promise<void> {
    await this.pool.query('DELETE FROM platform_settings WHERE key = $1', [key]);
  }

  public async listAdminUsers(): Promise<AdminUser[]> {
    const result = await this.pool.query<AdminUserRow>(
      `SELECT ${adminUserColumns} FROM admin_users ORDER BY created_at ASC`,
    );
    return result.rows.map(mapAdminUser);
  }

  public async getAdminUserByEmail(email: string): Promise<AdminUser | null> {
    const result = await this.pool.query<AdminUserRow>(
      `SELECT ${adminUserColumns} FROM admin_users WHERE email = lower($1)`,
      [email],
    );
    return result.rows[0] ? mapAdminUser(result.rows[0]) : null;
  }

  public async getAdminUserById(id: string): Promise<AdminUser | null> {
    const result = await this.pool.query<AdminUserRow>(
      `SELECT ${adminUserColumns} FROM admin_users WHERE id = $1`,
      [id],
    );
    return result.rows[0] ? mapAdminUser(result.rows[0]) : null;
  }

  public async createAdminUser(input: CreateAdminUserInput): Promise<AdminUser> {
    const result = await this.pool.query<AdminUserRow>(
      `INSERT INTO admin_users (id, email, name, password_hash)
       VALUES ($1, lower($2), $3, $4)
       RETURNING ${adminUserColumns}`,
      [randomUUID(), input.email, input.name, input.passwordHash],
    );
    return mapAdminUser(result.rows[0]!);
  }

  public async setAdminUserPassword(id: string, passwordHash: string): Promise<AdminUser | null> {
    const result = await this.pool.query<AdminUserRow>(
      `UPDATE admin_users SET password_hash = $2, updated_at = now()
       WHERE id = $1
       RETURNING ${adminUserColumns}`,
      [id, passwordHash],
    );
    return result.rows[0] ? mapAdminUser(result.rows[0]) : null;
  }

  public async recordAdminLogin(id: string): Promise<void> {
    await this.pool.query('UPDATE admin_users SET last_login_at = now() WHERE id = $1', [id]);
  }

  public async createAdminSession(input: CreateAdminSessionInput): Promise<AdminSession> {
    const result = await this.pool.query<AdminSessionRow>(
      `INSERT INTO admin_sessions (id, admin_user_id, token_hash, csrf_token, expires_at)
       VALUES ($1, $2, $3, $4, $5)
       RETURNING id, admin_user_id, csrf_token, expires_at, created_at`,
      [randomUUID(), input.adminUserId, input.tokenHash, input.csrfToken, input.expiresAt],
    );
    return mapAdminSession(result.rows[0]!);
  }

  public async findAdminSession(tokenHash: string): Promise<{ session: AdminSession; user: AdminUser } | null> {
    const result = await this.pool.query<{ session: AdminSessionRow; user: AdminUserRow }>(
      // Composite columns keep the two rows apart; both tables have `id` and `created_at`,
      // and a flat select would silently hand the session's timestamps to the user.
      `SELECT to_jsonb(s) - 'token_hash' AS session, to_jsonb(u) AS "user"
       FROM admin_sessions s
       JOIN admin_users u ON u.id = s.admin_user_id
       WHERE s.token_hash = $1 AND s.expires_at > now() AND u.active = true`,
      [tokenHash],
    );
    const row = result.rows[0];
    if (!row) {
      return null;
    }
    // jsonb renders timestamps as ISO strings, so they are revived here.
    const revive = (value: string | null): Date | null => (value === null ? null : new Date(value));
    return {
      session: {
        id: row.session.id,
        adminUserId: row.session.admin_user_id,
        csrfToken: row.session.csrf_token,
        expiresAt: new Date(row.session.expires_at as unknown as string),
        createdAt: new Date(row.session.created_at as unknown as string),
      },
      user: {
        id: row.user.id,
        email: row.user.email,
        name: row.user.name,
        passwordHash: row.user.password_hash,
        active: row.user.active,
        lastLoginAt: revive(row.user.last_login_at as unknown as string | null),
        createdAt: new Date(row.user.created_at as unknown as string),
        updatedAt: new Date(row.user.updated_at as unknown as string),
      },
    };
  }

  public async deleteAdminSession(tokenHash: string): Promise<void> {
    await this.pool.query('DELETE FROM admin_sessions WHERE token_hash = $1', [tokenHash]);
  }

  public async deleteAdminSessionsForUser(adminUserId: string): Promise<void> {
    await this.pool.query('DELETE FROM admin_sessions WHERE admin_user_id = $1', [adminUserId]);
  }

  public async deleteExpiredAdminSessions(): Promise<void> {
    await this.pool.query('DELETE FROM admin_sessions WHERE expires_at <= now()');
  }

  public async recordAuditEvent(input: RecordAuditEventInput): Promise<AuditEvent> {
    const result = await this.pool.query<AuditEventRow>(
      `INSERT INTO audit_events (
         id, actor_id, actor_label, action, entity_type, entity_id, summary, details
       ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8::jsonb)
       RETURNING ${auditEventColumns}`,
      [
        randomUUID(),
        input.actorId,
        input.actorLabel,
        input.action,
        input.entityType,
        input.entityId,
        input.summary,
        JSON.stringify(input.details ?? {}),
      ],
    );
    return mapAuditEvent(result.rows[0]!);
  }

  public async listAuditEvents(options: ListAuditEventsOptions): Promise<AuditEvent[]> {
    const result = await this.pool.query<AuditEventRow>(
      `SELECT ${auditEventColumns}
       FROM audit_events
       WHERE ($1::text IS NULL OR entity_type = $1)
         AND ($2::text IS NULL OR entity_id = $2)
       ORDER BY created_at DESC
       LIMIT $3 OFFSET $4`,
      [options.entityType ?? null, options.entityId ?? null, options.limit, options.offset],
    );
    return result.rows.map(mapAuditEvent);
  }
}

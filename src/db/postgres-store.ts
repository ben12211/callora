import { randomUUID } from 'node:crypto';
import type pg from 'pg';
import type {
  AgentConfig,
  AttachRealtimeSessionInput,
  Business,
  CallRecord,
  CreateBusinessInput,
  ListCallsOptions,
  UpdateBusinessInput,
  UpdateCallStatusInput,
  UpsertCallInput,
} from '../domain/models.js';
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
  enabled: boolean;
  created_at: Date;
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
    enabled: row.enabled,
    createdAt: row.created_at,
    updatedAt: row.updated_at,
  };
}

const agentConfigColumns = `
  business_id, instructions, greeting, language, voice, realtime_model, enabled,
  created_at, updated_at
`;

const callColumns = `
  id, business_id, twilio_call_sid, twilio_stream_sid, openai_session_id,
  from_number, to_number, status, direction,
  duration_seconds, started_at, ended_at, created_at, updated_at
`;

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
}

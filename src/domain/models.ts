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
  enabled: boolean;
  createdAt: Date;
  updatedAt: Date;
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

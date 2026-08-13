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

export interface DataStore {
  ping(): Promise<void>;
  listBusinesses(): Promise<Business[]>;
  getBusinessById(id: string): Promise<Business | null>;
  getBusinessByPhoneNumber(phoneNumber: string, activeOnly?: boolean): Promise<Business | null>;
  createBusiness(input: CreateBusinessInput): Promise<Business>;
  updateBusiness(id: string, input: UpdateBusinessInput): Promise<Business | null>;
  deleteBusiness(id: string): Promise<Business | null>;
  getAgentConfig(businessId: string): Promise<AgentConfig | null>;
  upsertCall(input: UpsertCallInput): Promise<CallRecord>;
  attachRealtimeSession(input: AttachRealtimeSessionInput): Promise<CallRecord | null>;
  updateCallStatus(input: UpdateCallStatusInput): Promise<CallRecord | null>;
  listCalls(options: ListCallsOptions): Promise<CallRecord[]>;
  getCallById(id: string): Promise<CallRecord | null>;
  getCallByTwilioSid(twilioCallSid: string): Promise<CallRecord | null>;
}

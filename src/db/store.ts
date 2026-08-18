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

export interface DataStore {
  ping(): Promise<void>;
  listBusinesses(): Promise<Business[]>;
  getBusinessById(id: string): Promise<Business | null>;
  getBusinessByPhoneNumber(phoneNumber: string, activeOnly?: boolean): Promise<Business | null>;
  createBusiness(input: CreateBusinessInput): Promise<Business>;
  updateBusiness(id: string, input: UpdateBusinessInput): Promise<Business | null>;
  deleteBusiness(id: string): Promise<Business | null>;
  getAgentConfig(businessId: string): Promise<AgentConfig | null>;
  upsertAgentConfig(businessId: string, input: UpsertAgentConfigInput): Promise<AgentConfig>;
  upsertCall(input: UpsertCallInput): Promise<CallRecord>;
  attachRealtimeSession(input: AttachRealtimeSessionInput): Promise<CallRecord | null>;
  updateCallStatus(input: UpdateCallStatusInput): Promise<CallRecord | null>;
  listCalls(options: ListCallsOptions): Promise<CallRecord[]>;
  getCallById(id: string): Promise<CallRecord | null>;
  getCallByTwilioSid(twilioCallSid: string): Promise<CallRecord | null>;
  countCalls(businessId?: string): Promise<number>;

  // Platform settings written from the dashboard; an absent key means "use the environment".
  listPlatformSettings(): Promise<PlatformSetting[]>;
  upsertPlatformSetting(input: UpsertPlatformSettingInput): Promise<PlatformSetting>;
  deletePlatformSetting(key: string): Promise<void>;

  // Control plane: administrators, sessions, and the audit trail.
  listAdminUsers(): Promise<AdminUser[]>;
  getAdminUserByEmail(email: string): Promise<AdminUser | null>;
  getAdminUserById(id: string): Promise<AdminUser | null>;
  createAdminUser(input: CreateAdminUserInput): Promise<AdminUser>;
  setAdminUserPassword(id: string, passwordHash: string): Promise<AdminUser | null>;
  recordAdminLogin(id: string): Promise<void>;
  createAdminSession(input: CreateAdminSessionInput): Promise<AdminSession>;
  /** Resolves a session cookie hash to its live session and owner, or null when expired. */
  findAdminSession(tokenHash: string): Promise<{ session: AdminSession; user: AdminUser } | null>;
  deleteAdminSession(tokenHash: string): Promise<void>;
  deleteAdminSessionsForUser(adminUserId: string): Promise<void>;
  deleteExpiredAdminSessions(): Promise<void>;
  recordAuditEvent(input: RecordAuditEventInput): Promise<AuditEvent>;
  listAuditEvents(options: ListAuditEventsOptions): Promise<AuditEvent[]>;
}

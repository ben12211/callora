import type {
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
  upsertCall(input: UpsertCallInput): Promise<CallRecord>;
  updateCallStatus(input: UpdateCallStatusInput): Promise<CallRecord | null>;
  listCalls(options: ListCallsOptions): Promise<CallRecord[]>;
  getCallById(id: string): Promise<CallRecord | null>;
}

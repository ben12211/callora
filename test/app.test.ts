import { randomUUID } from 'node:crypto';
import twilio from 'twilio';
import { beforeEach, describe, expect, it } from 'vitest';
import { buildApp } from '../src/app.js';
import type { AppConfig } from '../src/config.js';
import type { DataStore } from '../src/db/store.js';
import type {
  Business,
  CallRecord,
  CreateBusinessInput,
  ListCallsOptions,
  UpdateBusinessInput,
  UpdateCallStatusInput,
  UpsertCallInput,
} from '../src/domain/models.js';

const firstBusinessId = '00000000-0000-4000-8000-000000000001';
const secondBusinessId = '00000000-0000-4000-8000-000000000002';
const firstNumber = '+15551234567';
const secondNumber = '+15557654321';

function business(id: string, name: string, phoneNumber: string, greeting: string): Business {
  const now = new Date();
  return { id, name, phoneNumber, greeting, active: true, createdAt: now, updatedAt: now };
}

class MemoryStore implements DataStore {
  public businesses: Business[] = [
    business(firstBusinessId, 'First Business', firstNumber, 'Hello from the first business.'),
    business(secondBusinessId, 'Second Business', secondNumber, 'Hello from the second business.'),
  ];

  public calls: CallRecord[] = [];
  public healthy = true;

  public async ping(): Promise<void> {
    if (!this.healthy) {
      throw new Error('database unavailable');
    }
  }

  public async listBusinesses(): Promise<Business[]> {
    return this.businesses;
  }

  public async getBusinessById(id: string): Promise<Business | null> {
    return this.businesses.find((item) => item.id === id) ?? null;
  }

  public async getBusinessByPhoneNumber(phoneNumber: string, activeOnly = true): Promise<Business | null> {
    return this.businesses.find(
      (item) => item.phoneNumber === phoneNumber && (!activeOnly || item.active),
    ) ?? null;
  }

  public async createBusiness(input: CreateBusinessInput): Promise<Business> {
    const created = business(randomUUID(), input.name, input.phoneNumber, input.greeting);
    created.active = input.active;
    this.businesses.push(created);
    return created;
  }

  public async updateBusiness(id: string, input: UpdateBusinessInput): Promise<Business | null> {
    const existing = await this.getBusinessById(id);
    if (!existing) {
      return null;
    }
    Object.assign(existing, input, { updatedAt: new Date() });
    return existing;
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
}

const config: AppConfig = {
  nodeEnv: 'test',
  host: '127.0.0.1',
  port: 3000,
  logLevel: 'silent',
  databaseUrl: 'postgresql://unused',
  twilioAuthToken: 'test-auth-token',
  publicBaseUrl: 'https://voice.example.test',
};

function signedHeaders(path: string, payload: Record<string, string>): Record<string, string> {
  return {
    'content-type': 'application/x-www-form-urlencoded',
    'x-twilio-signature': twilio.getExpectedTwilioSignature(
      config.twilioAuthToken,
      `${config.publicBaseUrl}${path}`,
      payload,
    ),
  };
}

describe('Callora backend', () => {
  let store: MemoryStore;

  beforeEach(() => {
    store = new MemoryStore();
  });

  it('reports health and database failures', async () => {
    const app = await buildApp({ config, store });
    expect((await app.inject({ method: 'GET', url: '/health' })).statusCode).toBe(200);
    store.healthy = false;
    expect((await app.inject({ method: 'GET', url: '/health' })).statusCode).toBe(503);
    await app.close();
  });

  it('selects the tenant exclusively from the Twilio To number and stores the call', async () => {
    const app = await buildApp({ config, store });
    const path = '/webhooks/twilio/voice';
    const payload = {
      To: firstNumber,
      From: secondNumber,
      CallSid: 'CA123456789',
      CallStatus: 'ringing',
      Direction: 'inbound',
    };
    const response = await app.inject({
      method: 'POST',
      url: path,
      headers: signedHeaders(path, payload),
      payload: new URLSearchParams(payload).toString(),
    });

    expect(response.statusCode).toBe(200);
    expect(response.headers['content-type']).toContain('text/xml');
    expect(response.body).toContain('Hello from the first business.');
    expect(response.body).not.toContain('Hello from the second business.');
    expect(store.calls).toHaveLength(1);
    expect(store.calls[0]?.businessId).toBe(firstBusinessId);
    expect(store.calls[0]?.toNumber).toBe(firstNumber);
    await app.close();
  });

  it('rejects an invalid Twilio signature', async () => {
    const app = await buildApp({ config, store });
    const response = await app.inject({
      method: 'POST',
      url: '/webhooks/twilio/voice',
      headers: {
        'content-type': 'application/x-www-form-urlencoded',
        'x-twilio-signature': 'invalid',
      },
      payload: new URLSearchParams({ To: firstNumber, CallSid: 'CA123' }).toString(),
    });

    expect(response.statusCode).toBe(403);
    expect(store.calls).toHaveLength(0);
    await app.close();
  });

  it('updates a call from a signed status callback', async () => {
    const app = await buildApp({ config, store });
    await store.upsertCall({
      businessId: firstBusinessId,
      twilioCallSid: 'CASTATUS123',
      fromNumber: secondNumber,
      toNumber: firstNumber,
      status: 'ringing',
      direction: 'inbound',
    });
    store.businesses[0]!.active = false;
    const path = '/webhooks/twilio/call-status';
    const payload = {
      To: firstNumber,
      CallSid: 'CASTATUS123',
      CallStatus: 'completed',
      CallDuration: '42',
    };
    const response = await app.inject({
      method: 'POST',
      url: path,
      headers: signedHeaders(path, payload),
      payload: new URLSearchParams(payload).toString(),
    });

    expect(response.statusCode).toBe(204);
    expect(store.calls[0]?.status).toBe('completed');
    expect(store.calls[0]?.durationSeconds).toBe(42);
    await app.close();
  });

  it('supports business CRUD and call reads', async () => {
    const app = await buildApp({ config, store });
    const createResponse = await app.inject({
      method: 'POST',
      url: '/api/businesses',
      payload: {
        name: 'New Business',
        phoneNumber: '+15550001111',
        greeting: 'Welcome to New Business.',
      },
    });
    expect(createResponse.statusCode).toBe(201);
    const created = createResponse.json<{ data: Business }>().data;

    const updateResponse = await app.inject({
      method: 'PATCH',
      url: `/api/businesses/${created.id}`,
      payload: { active: false },
    });
    expect(updateResponse.statusCode).toBe(200);
    expect(updateResponse.json<{ data: Business }>().data.active).toBe(false);

    expect((await app.inject({ method: 'GET', url: '/api/calls?limit=10' })).statusCode).toBe(200);
    expect((await app.inject({ method: 'DELETE', url: `/api/businesses/${created.id}` })).statusCode).toBe(204);
    await app.close();
  });

  it('requires E.164 business phone numbers', async () => {
    const app = await buildApp({ config, store });
    const response = await app.inject({
      method: 'POST',
      url: '/api/businesses',
      payload: { name: 'Bad Number', phoneNumber: '555-1234', greeting: 'Hello' },
    });
    expect(response.statusCode).toBe(400);
    await app.close();
  });
});

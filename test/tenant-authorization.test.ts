import { describe, expect, it } from 'vitest';
import { buildApp } from '../src/app.js';
import { hashPassword } from '../src/auth/passwords.js';
import type { AppConfig } from '../src/config.js';
import {
  MemoryStore,
  firstBusinessId,
  secondBusinessId,
  testConfig,
} from './support/memory-store.js';

/**
 * Administration used to be authenticated but flat: every account reached every business,
 * every call, and every provider credential, so a business owner could not be given a
 * login at all.
 *
 * These tests are the isolation boundary. A tenant asking about a business that is not
 * theirs is answered 404 rather than 403 — whether another tenant exists on this platform
 * is not theirs to learn.
 */

const password = 'correct horse battery staple';

const config: AppConfig = {
  ...testConfig,
  auth: { ...testConfig.auth, apiKey: 'test-management-api-key-0123456789' },
};

async function seed(): Promise<MemoryStore> {
  const store = new MemoryStore();
  await store.createAdminUser({
    email: 'platform@callora.test',
    name: 'Platform Admin',
    passwordHash: await hashPassword(password),
  });
  await store.createAdminUser({
    email: 'owner@first.test',
    name: 'First Business Owner',
    passwordHash: await hashPassword(password),
    role: 'business',
    businessId: firstBusinessId,
  });
  return store;
}

async function signIn(
  app: Awaited<ReturnType<typeof buildApp>>,
  email: string,
): Promise<string> {
  const response = await app.inject({
    method: 'POST',
    url: '/dashboard/login',
    headers: { 'content-type': 'application/x-www-form-urlencoded' },
    payload: new URLSearchParams({ email, password }).toString(),
  });
  expect(response.statusCode).toBe(303);
  const raw = response.headers['set-cookie'];
  const cookie = Array.isArray(raw) ? raw[0] : raw;
  return cookie!.split(';')[0]!;
}

async function withApp<T>(
  run: (app: Awaited<ReturnType<typeof buildApp>>, store: MemoryStore) => Promise<T>,
): Promise<T> {
  const store = await seed();
  const app = await buildApp({ config, store });
  try {
    return await run(app, store);
  } finally {
    await app.close();
  }
}

describe('a business administrator is confined to their own tenant', () => {
  it('sees only their own business in the list', async () => {
    await withApp(async (app) => {
      const cookie = await signIn(app, 'owner@first.test');
      const response = await app.inject({ method: 'GET', url: '/api/businesses', headers: { cookie } });

      expect(response.statusCode).toBe(200);
      const body = response.json() as { data: { id: string }[] };
      expect(body.data.map((business) => business.id)).toEqual([firstBusinessId]);
    });
  });

  it('reads its own business and is told another does not exist', async () => {
    await withApp(async (app) => {
      const cookie = await signIn(app, 'owner@first.test');

      const own = await app.inject({
        method: 'GET',
        url: `/api/businesses/${firstBusinessId}`,
        headers: { cookie },
      });
      expect(own.statusCode).toBe(200);

      const other = await app.inject({
        method: 'GET',
        url: `/api/businesses/${secondBusinessId}`,
        headers: { cookie },
      });
      expect(other.statusCode).toBe(404);
    });
  });

  it('cannot read or write another tenant agent configuration', async () => {
    await withApp(async (app) => {
      const cookie = await signIn(app, 'owner@first.test');

      const read = await app.inject({
        method: 'GET',
        url: `/api/businesses/${secondBusinessId}/agent`,
        headers: { cookie },
      });
      expect(read.statusCode).toBe(404);

      const write = await app.inject({
        method: 'PUT',
        url: `/api/businesses/${secondBusinessId}/agent`,
        headers: { cookie, 'content-type': 'application/json' },
        payload: { enabled: true },
      });
      expect(write.statusCode).toBe(404);
    });
  });

  it('cannot create or delete a business', async () => {
    await withApp(async (app) => {
      const cookie = await signIn(app, 'owner@first.test');

      const created = await app.inject({
        method: 'POST',
        url: '/api/businesses',
        headers: { cookie, 'content-type': 'application/json' },
        payload: { name: 'Sneaky', phoneNumber: '+15550001111', greeting: 'Hello' },
      });
      expect(created.statusCode).toBe(403);

      const deleted = await app.inject({
        method: 'DELETE',
        url: `/api/businesses/${firstBusinessId}`,
        headers: { cookie },
      });
      expect(deleted.statusCode).toBe(403);
    });
  });

  it('cannot reach provider credentials or the metrics endpoint', async () => {
    await withApp(async (app) => {
      const cookie = await signIn(app, 'owner@first.test');

      expect((await app.inject({ method: 'GET', url: '/api/providers', headers: { cookie } })).statusCode).toBe(403);
      expect((await app.inject({ method: 'GET', url: '/metrics', headers: { cookie } })).statusCode).toBe(403);
      expect(
        (await app.inject({ method: 'GET', url: '/dashboard/providers', headers: { cookie } })).statusCode,
      ).toBe(403);
    });
  });

  it('sees only its own calls, whatever the query string asks for', async () => {
    await withApp(async (app, store) => {
      await store.upsertCall({
        businessId: firstBusinessId,
        twilioCallSid: 'CAFIRST',
        fromNumber: '+15551110000',
        toNumber: '+15551234567',
        status: 'completed',
        direction: 'inbound',
      });
      await store.upsertCall({
        businessId: secondBusinessId,
        twilioCallSid: 'CASECOND',
        fromNumber: '+15552220000',
        toNumber: '+15557654321',
        status: 'completed',
        direction: 'inbound',
      });

      const cookie = await signIn(app, 'owner@first.test');
      const response = await app.inject({
        method: 'GET',
        // Asking for the other tenant explicitly must not widen the scope.
        url: `/api/calls?businessId=${secondBusinessId}`,
        headers: { cookie },
      });

      expect(response.statusCode).toBe(200);
      const body = response.json() as { data: { twilioCallSid: string }[] };
      expect(body.data.map((call) => call.twilioCallSid)).toEqual(['CAFIRST']);
    });
  });

  it('cannot read another tenant call or its transcript', async () => {
    await withApp(async (app, store) => {
      const call = await store.upsertCall({
        businessId: secondBusinessId,
        twilioCallSid: 'CASECOND',
        fromNumber: '+15552220000',
        toNumber: '+15557654321',
        status: 'completed',
        direction: 'inbound',
      });
      await store.appendTranscriptTurn({
        callId: call.id,
        businessId: secondBusinessId,
        speaker: 'caller',
        content: 'something confidential',
        turn: 1,
      });

      const cookie = await signIn(app, 'owner@first.test');
      expect(
        (await app.inject({ method: 'GET', url: `/api/calls/${call.id}`, headers: { cookie } })).statusCode,
      ).toBe(404);
      expect(
        (await app.inject({ method: 'GET', url: `/api/calls/${call.id}/transcript`, headers: { cookie } }))
          .statusCode,
      ).toBe(404);
      expect(
        (await app.inject({ method: 'GET', url: `/dashboard/calls/${call.id}`, headers: { cookie } }))
          .statusCode,
      ).toBe(404);
    });
  });

  it('reports its own scope at /api/me', async () => {
    await withApp(async (app) => {
      const cookie = await signIn(app, 'owner@first.test');
      const response = await app.inject({ method: 'GET', url: '/api/me', headers: { cookie } });

      expect(response.json()).toMatchObject({
        data: { role: 'business', businessId: firstBusinessId },
      });
    });
  });

  it('is not offered the navigation it cannot open', async () => {
    await withApp(async (app) => {
      const cookie = await signIn(app, 'owner@first.test');
      const response = await app.inject({ method: 'GET', url: '/dashboard', headers: { cookie } });

      expect(response.statusCode).toBe(200);
      expect(response.body).not.toContain('href="/dashboard/providers"');
      expect(response.body).toContain('href="/dashboard/calls"');
    });
  });
});

describe('platform administrators and the machine credential are unchanged', () => {
  it('still reach every business', async () => {
    await withApp(async (app) => {
      const cookie = await signIn(app, 'platform@callora.test');
      const response = await app.inject({ method: 'GET', url: '/api/businesses', headers: { cookie } });

      const body = response.json() as { data: { id: string }[] };
      expect(body.data.map((business) => business.id).sort()).toEqual(
        [firstBusinessId, secondBusinessId].sort(),
      );
    });
  });

  it('still reach providers, metrics, and every tenant through the API key', async () => {
    await withApp(async (app) => {
      const headers = { 'x-api-key': 'test-management-api-key-0123456789' };

      expect((await app.inject({ method: 'GET', url: '/api/providers', headers })).statusCode).toBe(200);
      expect((await app.inject({ method: 'GET', url: '/metrics', headers })).statusCode).toBe(200);
      expect(
        (await app.inject({ method: 'GET', url: `/api/businesses/${secondBusinessId}`, headers })).statusCode,
      ).toBe(200);
    });
  });
});

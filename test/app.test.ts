import twilio from 'twilio';
import { beforeEach, describe, expect, it } from 'vitest';
import { buildApp } from '../src/app.js';
import { createCallerAllowlist } from '../src/dev/caller-allowlist.js';
import { verifyStreamToken } from '../src/http/stream-token.js';
import type { Business } from '../src/domain/models.js';
import {
  MemoryStore,
  agentConfig,
  firstBusinessId,
  firstNumber,
  secondNumber,
  testConfig,
} from './support/memory-store.js';

const config = { ...testConfig, auth: { ...testConfig.auth, apiKey: 'test-management-api-key' } };

/** The management API is authenticated; these tests use the machine credential. */
const apiHeaders = { 'x-api-key': 'test-management-api-key' };

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

  it('returns a bidirectional Connect/Stream TwiML when the business has an enabled agent', async () => {
    store.agentConfigs.push(agentConfig(firstBusinessId));
    const app = await buildApp({ config, store });
    const path = '/webhooks/twilio/voice';
    const payload = {
      To: firstNumber,
      From: secondNumber,
      CallSid: 'CAREALTIME1',
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
    expect(response.body).toContain('<Connect>');
    expect(response.body).toContain('<Stream');
    expect(response.body).not.toContain('<Start>');
    expect(response.body).not.toContain('Hello from the first business.');

    const streamUrl = /url="([^"]+)"/.exec(response.body)?.[1]?.replaceAll('&amp;', '&');
    expect(streamUrl).toBeDefined();
    const parsed = new URL(streamUrl!);
    expect(parsed.protocol).toBe('wss:');
    expect(parsed.pathname).toBe('/webhooks/twilio/media');
    expect(parsed.search).toBe('');
    const encodedToken = /<Parameter name="token" value="([^"]+)"\/>/.exec(response.body)?.[1];
    expect(encodedToken).toBeDefined();
    const claims = verifyStreamToken(config.twilioAuthToken, encodedToken ?? '');
    expect(claims).toEqual(
      expect.objectContaining({ callSid: 'CAREALTIME1', businessId: firstBusinessId }),
    );
    expect(store.calls[0]?.businessId).toBe(firstBusinessId);
    await app.close();
  });

  it('rejects a caller that is not on the development allowlist, before any stream token', async () => {
    store.agentConfigs.push(agentConfig(firstBusinessId));
    const { allowlist } = createCallerAllowlist([secondNumber]);
    const app = await buildApp({ config, store, callerAllowlist: allowlist });
    const path = '/webhooks/twilio/voice';
    const payload = {
      To: firstNumber,
      From: '+15550009999',
      CallSid: 'CABLOCKED',
      CallStatus: 'ringing',
    };
    const response = await app.inject({
      method: 'POST',
      url: path,
      headers: signedHeaders(path, payload),
      payload: new URLSearchParams(payload).toString(),
    });

    expect(response.statusCode).toBe(200);
    expect(response.body).toContain('<Hangup');
    expect(response.body).not.toContain('<Connect>');
    expect(response.body).not.toContain('<Parameter');
    // No token was minted, so the media stream and OpenAI session are unreachable.
    expect(response.body).not.toContain('token');
    expect(store.calls).toHaveLength(0);
    await app.close();
  });

  it('lets an allowlisted caller through and still routes on To alone', async () => {
    store.agentConfigs.push(agentConfig(firstBusinessId));
    const { allowlist } = createCallerAllowlist([`${secondNumber}`]);
    const app = await buildApp({ config, store, callerAllowlist: allowlist });
    const path = '/webhooks/twilio/voice';
    const payload = {
      To: firstNumber,
      From: secondNumber,
      CallSid: 'CAALLOWED',
      CallStatus: 'ringing',
    };
    const response = await app.inject({
      method: 'POST',
      url: path,
      headers: signedHeaders(path, payload),
      payload: new URLSearchParams(payload).toString(),
    });

    expect(response.statusCode).toBe(200);
    expect(response.body).toContain('<Connect>');
    // secondNumber is also a business number, but From must never select the tenant.
    expect(store.calls[0]?.businessId).toBe(firstBusinessId);
    await app.close();
  });

  it('allows every caller when no allowlist is configured', async () => {
    store.agentConfigs.push(agentConfig(firstBusinessId));
    const { allowlist } = createCallerAllowlist([]);
    const app = await buildApp({ config, store, callerAllowlist: allowlist });
    const path = '/webhooks/twilio/voice';
    const payload = { To: firstNumber, From: '+15550009999', CallSid: 'CAOPEN', CallStatus: 'ringing' };
    const response = await app.inject({
      method: 'POST',
      url: path,
      headers: signedHeaders(path, payload),
      payload: new URLSearchParams(payload).toString(),
    });

    expect(response.body).toContain('<Connect>');
    await app.close();
  });

  it('falls back to the static greeting when the agent is disabled', async () => {
    store.agentConfigs.push(agentConfig(firstBusinessId, false));
    const app = await buildApp({ config, store });
    const path = '/webhooks/twilio/voice';
    const payload = { To: firstNumber, From: secondNumber, CallSid: 'CADISABLED', CallStatus: 'ringing' };
    const response = await app.inject({
      method: 'POST',
      url: path,
      headers: signedHeaders(path, payload),
      payload: new URLSearchParams(payload).toString(),
    });

    expect(response.body).toContain('Hello from the first business.');
    expect(response.body).not.toContain('<Connect>');
    await app.close();
  });

  it('rejects a media stream handshake without a valid Twilio signature', async () => {
    const app = await buildApp({ config, store });
    const rejected = await app.inject({
      method: 'GET',
      url: '/webhooks/twilio/media',
      headers: { connection: 'upgrade', upgrade: 'websocket', 'sec-websocket-version': '13', 'sec-websocket-key': 'dGhlIHNhbXBsZSBub25jZQ==' },
    });
    expect(rejected.statusCode).toBe(403);
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
      headers: apiHeaders,
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
      headers: apiHeaders,
      payload: { active: false },
    });
    expect(updateResponse.statusCode).toBe(200);
    expect(updateResponse.json<{ data: Business }>().data.active).toBe(false);

    expect(
      (await app.inject({ method: 'GET', url: '/api/calls?limit=10', headers: apiHeaders })).statusCode,
    ).toBe(200);
    expect(
      (await app.inject({ method: 'DELETE', url: `/api/businesses/${created.id}`, headers: apiHeaders }))
        .statusCode,
    ).toBe(204);
    await app.close();
  });

  it('requires E.164 business phone numbers', async () => {
    const app = await buildApp({ config, store });
    const response = await app.inject({
      method: 'POST',
      url: '/api/businesses',
      headers: apiHeaders,
      payload: { name: 'Bad Number', phoneNumber: '555-1234', greeting: 'Hello' },
    });
    expect(response.statusCode).toBe(400);
    await app.close();
  });
});

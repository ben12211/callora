import twilio from 'twilio';
import { beforeEach, describe, expect, it } from 'vitest';
import { buildApp } from '../src/app.js';
import { ensureBootstrapAdmin } from '../src/auth/bootstrap.js';
import { hashPassword, verifyPassword } from '../src/auth/passwords.js';
import type { AppConfig } from '../src/config.js';
import type { AgentConfig, Business } from '../src/domain/models.js';
import { providerStatuses } from '../src/realtime/provider-catalog.js';
import {
  MemoryStore,
  agentConfig,
  firstBusinessId,
  firstNumber,
  secondNumber,
  testConfig,
} from './support/memory-store.js';

const password = 'correct horse battery staple';

const config: AppConfig = {
  ...testConfig,
  auth: { ...testConfig.auth, apiKey: 'test-management-api-key-0123456789' },
};

const silentLogger = { info: () => undefined, warn: () => undefined };

async function withAdmin(store: MemoryStore): Promise<void> {
  await store.createAdminUser({
    email: 'admin@callora.test',
    name: 'Admin',
    passwordHash: await hashPassword(password),
  });
}

/** Signs in through the dashboard form and returns the session cookie header value. */
async function signIn(
  app: Awaited<ReturnType<typeof buildApp>>,
  email = 'admin@callora.test',
  secret = password,
): Promise<string> {
  const response = await app.inject({
    method: 'POST',
    url: '/dashboard/login',
    headers: { 'content-type': 'application/x-www-form-urlencoded' },
    payload: new URLSearchParams({ email, password: secret }).toString(),
  });
  expect(response.statusCode).toBe(303);
  const raw = response.headers['set-cookie'];
  const cookie = Array.isArray(raw) ? raw[0] : raw;
  expect(cookie).toBeDefined();
  return cookie!.split(';')[0]!;
}

/** The CSRF token every dashboard form carries, read back out of the rendered page. */
function csrfFrom(body: string): string {
  const match = /name="_csrf" value="([^"]+)"/.exec(body);
  expect(match).not.toBeNull();
  return match![1]!;
}

function form(fields: Record<string, string>): { headers: Record<string, string>; payload: string } {
  return {
    headers: { 'content-type': 'application/x-www-form-urlencoded' },
    payload: new URLSearchParams(fields).toString(),
  };
}

describe('admin authentication', () => {
  let store: MemoryStore;

  beforeEach(async () => {
    store = new MemoryStore();
    await withAdmin(store);
  });

  it('hashes passwords so the plaintext is never stored', async () => {
    const encoded = await hashPassword(password);
    expect(encoded).not.toContain(password);
    expect(encoded.startsWith('scrypt$')).toBe(true);
    expect(await verifyPassword(password, encoded)).toBe(true);
    expect(await verifyPassword('wrong password entirely', encoded)).toBe(false);
    // Two hashes of the same password differ: the salt is per hash.
    expect(await hashPassword(password)).not.toBe(encoded);
  });

  it('refuses the management API without a credential', async () => {
    const app = await buildApp({ config, store });
    for (const url of ['/api/businesses', '/api/calls', '/api/providers', '/api/audit']) {
      const response = await app.inject({ method: 'GET', url });
      expect(response.statusCode, url).toBe(401);
    }
    const write = await app.inject({
      method: 'POST',
      url: '/api/businesses',
      payload: { name: 'Sneaky', phoneNumber: '+15550002222', greeting: 'Hi' },
    });
    expect(write.statusCode).toBe(401);
    expect(store.businesses).toHaveLength(2);
    await app.close();
  });

  it('accepts the platform API key and a dashboard session alike', async () => {
    const app = await buildApp({ config, store });
    const byKey = await app.inject({
      method: 'GET',
      url: '/api/businesses',
      headers: { 'x-api-key': 'test-management-api-key-0123456789' },
    });
    expect(byKey.statusCode).toBe(200);

    const cookie = await signIn(app);
    const bySession = await app.inject({ method: 'GET', url: '/api/businesses', headers: { cookie } });
    expect(bySession.statusCode).toBe(200);

    const wrongKey = await app.inject({
      method: 'GET',
      url: '/api/businesses',
      headers: { 'x-api-key': 'test-management-api-key-000000000' },
    });
    expect(wrongKey.statusCode).toBe(401);
    await app.close();
  });

  it('rejects bad credentials without revealing whether the account exists', async () => {
    const app = await buildApp({ config, store });
    const unknown = await app.inject({
      method: 'POST',
      url: '/dashboard/login',
      ...form({ email: 'nobody@callora.test', password: 'whatever-it-is' }),
    });
    const wrongPassword = await app.inject({
      method: 'POST',
      url: '/dashboard/login',
      ...form({ email: 'admin@callora.test', password: 'not-the-password' }),
    });

    expect(unknown.statusCode).toBe(401);
    expect(wrongPassword.statusCode).toBe(401);
    expect(unknown.body).toContain('Those credentials are not valid.');
    expect(wrongPassword.body).toContain('Those credentials are not valid.');
    expect(unknown.headers['set-cookie']).toBeUndefined();
    await app.close();
  });

  it('sends unauthenticated dashboard visitors to the login page', async () => {
    const app = await buildApp({ config, store });
    for (const url of ['/dashboard', '/dashboard/businesses', '/dashboard/calls', '/dashboard/settings']) {
      const response = await app.inject({ method: 'GET', url });
      expect(response.statusCode, url).toBe(303);
      expect(response.headers['location']).toBe('/dashboard/login');
    }
    await app.close();
  });

  it('ends the session on sign-out', async () => {
    const app = await buildApp({ config, store });
    const cookie = await signIn(app);
    expect((await app.inject({ method: 'GET', url: '/dashboard', headers: { cookie } })).statusCode).toBe(200);

    const page = await app.inject({ method: 'GET', url: '/dashboard', headers: { cookie } });
    await app.inject({
      method: 'POST',
      url: '/dashboard/logout',
      headers: { cookie, 'content-type': 'application/x-www-form-urlencoded' },
      payload: new URLSearchParams({ _csrf: csrfFrom(page.body) }).toString(),
    });

    const after = await app.inject({ method: 'GET', url: '/dashboard', headers: { cookie } });
    expect(after.statusCode).toBe(303);
    await app.close();
  });

  it('rejects a dashboard write whose CSRF token is missing or wrong', async () => {
    const app = await buildApp({ config, store });
    const cookie = await signIn(app);
    const response = await app.inject({
      method: 'POST',
      url: '/dashboard/businesses',
      headers: { cookie, 'content-type': 'application/x-www-form-urlencoded' },
      payload: new URLSearchParams({
        _csrf: 'not-the-session-token',
        name: 'Forged',
        phoneNumber: '+15550003333',
        greeting: 'Hello',
      }).toString(),
    });
    expect(response.statusCode).toBe(400);
    expect(store.businesses).toHaveLength(2);
    await app.close();
  });

  it('creates the bootstrap administrator from the environment and resets a changed password', async () => {
    const empty = new MemoryStore();
    const auth = { ...config.auth, bootstrapEmail: 'boot@callora.test', bootstrapPassword: 'a-long-boot-password' };
    await ensureBootstrapAdmin(empty, auth, silentLogger);
    expect(empty.admins).toHaveLength(1);
    expect(await verifyPassword('a-long-boot-password', empty.admins[0]!.passwordHash)).toBe(true);

    // Running again is idempotent: the same password leaves the hash alone.
    const hashBefore = empty.admins[0]!.passwordHash;
    await ensureBootstrapAdmin(empty, auth, silentLogger);
    expect(empty.admins).toHaveLength(1);
    expect(empty.admins[0]!.passwordHash).toBe(hashBefore);

    await ensureBootstrapAdmin(empty, { ...auth, bootstrapPassword: 'a-different-password' }, silentLogger);
    expect(await verifyPassword('a-different-password', empty.admins[0]!.passwordHash)).toBe(true);
  });
});

describe('business and agent management', () => {
  let store: MemoryStore;

  beforeEach(async () => {
    store = new MemoryStore();
    await withAdmin(store);
  });

  it('walks the whole flow: sign in, create a business, configure the agent, take the call', async () => {
    const everyProvider: AppConfig = {
      ...config,
      providers: {
        ...config.providers,
        elevenlabs: { apiKey: 'xi-test', agentId: 'agent_123', apiBaseUrl: 'https://api.elevenlabs.io' },
      },
    };
    const app = await buildApp({ config: everyProvider, store });
    const cookie = await signIn(app);

    const newPage = await app.inject({ method: 'GET', url: '/dashboard/businesses/new', headers: { cookie } });
    const csrf = csrfFrom(newPage.body);

    const created = await app.inject({
      method: 'POST',
      url: '/dashboard/businesses',
      headers: { cookie, 'content-type': 'application/x-www-form-urlencoded' },
      payload: new URLSearchParams({
        _csrf: csrf,
        name: 'Cafe Nof',
        phoneNumber: '+15550004444',
        greeting: 'Thanks for calling Cafe Nof.',
        active: 'on',
      }).toString(),
    });
    expect(created.statusCode).toBe(303);
    const business = store.businesses.find((item) => item.phoneNumber === '+15550004444');
    expect(business).toBeDefined();
    // The agent row exists immediately but stays off, so the number keeps answering with
    // the static greeting until it is configured.
    expect(store.agentConfigs.find((agent) => agent.businessId === business!.id)?.enabled).toBe(false);

    const saved = await app.inject({
      method: 'POST',
      url: `/dashboard/businesses/${business!.id}/agent`,
      headers: { cookie, 'content-type': 'application/x-www-form-urlencoded' },
      payload: new URLSearchParams({
        _csrf: csrf,
        enabled: 'on',
        language: 'he-IL',
        greeting: 'שלום, הגעתם לקפה נוף.',
        instructions: 'Answer questions about the cafe.',
        voiceProvider: 'elevenlabs',
        voice: 'voice_abc123',
        realtimeModel: 'eleven_turbo_v2_5',
      }).toString(),
    });
    expect(saved.statusCode).toBe(303);

    const agent = store.agentConfigs.find((item) => item.businessId === business!.id)!;
    expect(agent).toMatchObject({
      enabled: true,
      voiceProvider: 'elevenlabs',
      voice: 'voice_abc123',
      language: 'he-IL',
    });

    // Calling the number now bridges to the media stream instead of speaking the greeting.
    const path = '/webhooks/twilio/voice';
    const payload = { To: '+15550004444', From: secondNumber, CallSid: 'CAFLOW1', CallStatus: 'ringing' };
    const call = await app.inject({
      method: 'POST',
      url: path,
      headers: {
        'content-type': 'application/x-www-form-urlencoded',
        'x-twilio-signature': twilio.getExpectedTwilioSignature(
          config.twilioAuthToken,
          `${config.publicBaseUrl}${path}`,
          payload,
        ),
      },
      payload: new URLSearchParams(payload).toString(),
    });
    expect(call.body).toContain('<Connect>');
    expect(call.body).not.toContain('Thanks for calling Cafe Nof.');
    await app.close();
  });

  it('records an audit event for every important change', async () => {
    const app = await buildApp({ config, store });
    const cookie = await signIn(app);
    const page = await app.inject({ method: 'GET', url: '/dashboard/businesses/new', headers: { cookie } });
    const csrf = csrfFrom(page.body);

    await app.inject({
      method: 'POST',
      url: '/dashboard/businesses',
      headers: { cookie, 'content-type': 'application/x-www-form-urlencoded' },
      payload: new URLSearchParams({
        _csrf: csrf,
        name: 'Audited Business',
        phoneNumber: '+15550005555',
        greeting: 'Hello.',
        active: 'on',
      }).toString(),
    });
    const business = store.businesses.find((item) => item.phoneNumber === '+15550005555')!;

    // Unticking `active` disables the business, which is recorded as its own action.
    await app.inject({
      method: 'POST',
      url: `/dashboard/businesses/${business.id}`,
      headers: { cookie, 'content-type': 'application/x-www-form-urlencoded' },
      payload: new URLSearchParams({
        _csrf: csrf,
        name: 'Audited Business',
        phoneNumber: '+15550005555',
        greeting: 'Hello.',
      }).toString(),
    });

    const actions = store.auditEvents.map((event) => event.action);
    expect(actions).toContain('admin.logged_in');
    expect(actions).toContain('business.created');
    expect(actions).toContain('business.disabled');
    expect(store.businesses.find((item) => item.id === business.id)?.active).toBe(false);

    const audit = store.auditEvents.find((event) => event.action === 'business.disabled')!;
    expect(audit.actorLabel).toBe('admin@callora.test');
    expect(audit.entityId).toBe(business.id);

    const listed = await app.inject({ method: 'GET', url: '/dashboard/audit', headers: { cookie } });
    expect(listed.body).toContain('business.disabled');
    await app.close();
  });

  it('refuses to enable an agent on a provider the platform cannot execute', async () => {
    const app = await buildApp({ config, store });
    const response = await app.inject({
      method: 'PUT',
      url: `/api/businesses/${firstBusinessId}/agent`,
      headers: { 'x-api-key': config.auth.apiKey! },
      payload: {
        enabled: true,
        language: 'he-IL',
        greeting: 'שלום',
        instructions: 'Answer questions.',
        voiceProvider: 'cartesia',
        voice: '',
        realtimeModel: 'gpt-4o-mini',
      },
    });
    expect(response.statusCode).toBe(422);
    expect(response.json<{ error: string }>().error).toContain('cartesia');
    expect(store.agentConfigs).toHaveLength(0);
    await app.close();
  });

  it('saves a disabled agent on an unconfigured provider so it can be prepared in advance', async () => {
    const app = await buildApp({ config, store });
    const response = await app.inject({
      method: 'PUT',
      url: `/api/businesses/${firstBusinessId}/agent`,
      headers: { 'x-api-key': config.auth.apiKey! },
      payload: {
        enabled: false,
        language: 'en-US',
        greeting: 'Hello there.',
        instructions: 'Answer questions.',
        voiceProvider: 'cartesia',
        voice: '',
        realtimeModel: 'gpt-4o-mini',
      },
    });
    expect(response.statusCode).toBe(200);
    expect(response.json<{ data: AgentConfig }>().data.voiceProvider).toBe('cartesia');
    await app.close();
  });

  it('requires a voice for OpenAI but accepts a blank one elsewhere', async () => {
    const app = await buildApp({ config, store });
    const body = {
      enabled: false,
      language: 'en-US',
      greeting: 'Hello there.',
      instructions: 'Answer questions.',
      voice: '',
      realtimeModel: 'gpt-realtime-2.1',
    };
    const openai = await app.inject({
      method: 'PUT',
      url: `/api/businesses/${firstBusinessId}/agent`,
      headers: { 'x-api-key': config.auth.apiKey! },
      payload: { ...body, voiceProvider: 'openai' },
    });
    expect(openai.statusCode).toBe(400);

    // ElevenLabs keeps the voice configured on its own agent when this is blank.
    const elevenlabs = await app.inject({
      method: 'PUT',
      url: `/api/businesses/${firstBusinessId}/agent`,
      headers: { 'x-api-key': config.auth.apiKey! },
      payload: { ...body, voiceProvider: 'elevenlabs' },
    });
    expect(elevenlabs.statusCode).toBe(200);
    await app.close();
  });

  it('creates every business with an agent row through the API too', async () => {
    const app = await buildApp({ config, store });
    const response = await app.inject({
      method: 'POST',
      url: '/api/businesses',
      headers: { 'x-api-key': config.auth.apiKey! },
      payload: { name: 'API Business', phoneNumber: '+15550006666', greeting: 'Hello.' },
    });
    expect(response.statusCode).toBe(201);
    const created = response.json<{ data: Business & { agent: AgentConfig } }>().data;
    expect(created.agent.enabled).toBe(false);
    expect(created.agent.voiceProvider).toBe('openai');
    await app.close();
  });
});

describe('per-business provider selection', () => {
  let store: MemoryStore;

  beforeEach(async () => {
    store = new MemoryStore();
    await withAdmin(store);
  });

  it('answers with the static greeting when the chosen provider has no credentials', async () => {
    // The agent is enabled on Cartesia, but this deployment only holds OpenAI credentials.
    store.agentConfigs.push({ ...agentConfig(firstBusinessId), voiceProvider: 'cartesia' });
    const app = await buildApp({ config, store });
    const path = '/webhooks/twilio/voice';
    const payload = { To: firstNumber, From: secondNumber, CallSid: 'CAUNAVAILABLE', CallStatus: 'ringing' };
    const response = await app.inject({
      method: 'POST',
      url: path,
      headers: {
        'content-type': 'application/x-www-form-urlencoded',
        'x-twilio-signature': twilio.getExpectedTwilioSignature(
          config.twilioAuthToken,
          `${config.publicBaseUrl}${path}`,
          payload,
        ),
      },
      payload: new URLSearchParams(payload).toString(),
    });

    expect(response.body).toContain('Hello from the first business.');
    expect(response.body).not.toContain('<Connect>');
    await app.close();
  });

  it('reports which providers the platform can execute, without exposing credentials', async () => {
    const app = await buildApp({ config, store });
    const response = await app.inject({
      method: 'GET',
      url: '/api/providers',
      headers: { 'x-api-key': config.auth.apiKey! },
    });
    expect(response.statusCode).toBe(200);
    const data = response.json<{ data: { id: string; configured: boolean }[] }>().data;
    expect(data.map((provider) => provider.id)).toEqual(['openai', 'elevenlabs', 'cartesia']);
    expect(data.find((provider) => provider.id === 'openai')?.configured).toBe(true);
    expect(data.find((provider) => provider.id === 'cartesia')?.configured).toBe(false);
    expect(JSON.stringify(data)).not.toContain('test-openai-key');

    const page = await app.inject({
      method: 'GET',
      url: '/dashboard/providers',
      headers: { cookie: await signIn(app) },
    });
    expect(page.body).toContain('OpenAI Realtime');
    expect(page.body).toContain('Cartesia');
    expect(page.body).not.toContain('test-openai-key');
    await app.close();
  });

  it('marks a provider configured only when every credential it needs is present', () => {
    const partial = providerStatuses(
      {
        openai: null,
        elevenlabs: null,
        cartesia: null,
      },
      'openai',
      { ELEVENLABS_API_KEY: 'xi-test' },
    );
    const elevenlabs = partial.find((provider) => provider.id === 'elevenlabs')!;
    expect(elevenlabs.configured).toBe(false);
    // The key that is present is not reported as missing; the agent id still is.
    expect(elevenlabs.missingEnvironment).toEqual(['ELEVENLABS_AGENT_ID']);
  });
});

describe('dashboard pages', () => {
  let store: MemoryStore;

  beforeEach(async () => {
    store = new MemoryStore();
    await withAdmin(store);
  });

  it('renders the home, business, call, and settings pages', async () => {
    store.agentConfigs.push(agentConfig(firstBusinessId));
    await store.upsertCall({
      businessId: firstBusinessId,
      twilioCallSid: 'CAPAGE1',
      fromNumber: secondNumber,
      toNumber: firstNumber,
      status: 'completed',
      direction: 'inbound',
    });
    const app = await buildApp({ config, store });
    const cookie = await signIn(app);

    const home = await app.inject({ method: 'GET', url: '/dashboard', headers: { cookie } });
    expect(home.statusCode).toBe(200);
    expect(home.body).toContain('First Business');
    expect(home.headers['cache-control']).toBe('no-store');

    const detail = await app.inject({
      method: 'GET',
      url: `/dashboard/businesses/${firstBusinessId}`,
      headers: { cookie },
    });
    expect(detail.body).toContain('Agent configuration');
    expect(detail.body).toContain('System / custom instructions');
    expect(detail.body).toContain('voiceProvider');

    const calls = await app.inject({ method: 'GET', url: '/dashboard/calls', headers: { cookie } });
    expect(calls.body).toContain('CAPAGE1'.slice(0, 0) + firstNumber);

    const callDetail = await app.inject({
      method: 'GET',
      url: `/dashboard/calls/${store.calls[0]!.id}`,
      headers: { cookie },
    });
    expect(callDetail.body).toContain('CAPAGE1');

    const settings = await app.inject({ method: 'GET', url: '/dashboard/settings', headers: { cookie } });
    expect(settings.body).toContain('admin@callora.test');
    // The platform section names providers but never their credentials.
    expect(settings.body).not.toContain('test-openai-key');
    expect(settings.body).not.toContain(config.auth.apiKey!);
    await app.close();
  });

  it('escapes tenant text rather than rendering it as markup', async () => {
    store.businesses[0]!.name = '<script>alert(1)</script>';
    const app = await buildApp({ config, store });
    const cookie = await signIn(app);
    const response = await app.inject({ method: 'GET', url: '/dashboard/businesses', headers: { cookie } });
    expect(response.body).not.toContain('<script>alert(1)</script>');
    expect(response.body).toContain('&lt;script&gt;alert(1)&lt;/script&gt;');
    await app.close();
  });

  it('rotates the password and signs every session out', async () => {
    const app = await buildApp({ config, store });
    const cookie = await signIn(app);
    const settings = await app.inject({ method: 'GET', url: '/dashboard/settings', headers: { cookie } });

    const changed = await app.inject({
      method: 'POST',
      url: '/dashboard/settings/password',
      headers: { cookie, 'content-type': 'application/x-www-form-urlencoded' },
      payload: new URLSearchParams({
        _csrf: csrfFrom(settings.body),
        currentPassword: password,
        newPassword: 'an-even-longer-password',
        confirmPassword: 'an-even-longer-password',
      }).toString(),
    });
    expect(changed.statusCode).toBe(303);
    expect(changed.headers['location']).toBe('/dashboard/login');

    // The old cookie no longer works, and the new password does.
    expect((await app.inject({ method: 'GET', url: '/dashboard', headers: { cookie } })).statusCode).toBe(303);
    const fresh = await signIn(app, 'admin@callora.test', 'an-even-longer-password');
    expect((await app.inject({ method: 'GET', url: '/dashboard', headers: { cookie: fresh } })).statusCode).toBe(
      200,
    );
    expect(store.auditEvents.some((event) => event.action === 'admin.password_changed')).toBe(true);
    await app.close();
  });
});

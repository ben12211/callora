import twilio from 'twilio';
import { beforeEach, describe, expect, it } from 'vitest';
import { buildApp } from '../src/app.js';
import { hashPassword } from '../src/auth/passwords.js';
import type { AppConfig } from '../src/config.js';
import { SecretBox } from '../src/platform/secret-box.js';
import { PlatformSettings, platformValuesFromConfig } from '../src/platform/settings.js';
import {
  MemoryStore,
  agentConfig,
  firstBusinessId,
  firstNumber,
  secondNumber,
  testConfig,
} from './support/memory-store.js';

const password = 'correct horse battery staple';
const secretsKey = 'test-secrets-key-0123456789';

/** A deployment that starts with no provider credentials at all: everything comes from the UI. */
const bareConfig: AppConfig = {
  ...testConfig,
  secretsKey,
  providers: { openai: null, elevenlabs: null, cartesia: null },
};

/** The same deployment, but with the OpenAI key supplied by the environment. */
const envConfig: AppConfig = { ...testConfig, secretsKey };

const silentLogger = {
  info: () => undefined,
  warn: () => undefined,
  error: () => undefined,
};

async function withAdmin(store: MemoryStore): Promise<void> {
  await store.createAdminUser({
    email: 'admin@callora.test',
    name: 'Admin',
    passwordHash: await hashPassword(password),
  });
}

async function signIn(app: Awaited<ReturnType<typeof buildApp>>): Promise<string> {
  const response = await app.inject({
    method: 'POST',
    url: '/dashboard/login',
    headers: { 'content-type': 'application/x-www-form-urlencoded' },
    payload: new URLSearchParams({ email: 'admin@callora.test', password }).toString(),
  });
  const raw = response.headers['set-cookie'];
  const cookie = Array.isArray(raw) ? raw[0] : raw;
  return cookie!.split(';')[0]!;
}

function csrfFrom(body: string): string {
  const match = /name="_csrf" value="([^"]+)"/.exec(body);
  expect(match).not.toBeNull();
  return match![1]!;
}

function revisionFrom(body: string): string {
  const match = /name="revision" value="([^"]+)"/.exec(body);
  expect(match).not.toBeNull();
  return match![1]!;
}

/** Signs in, opens the providers page, and posts the given fields back to it. */
async function saveSettings(
  app: Awaited<ReturnType<typeof buildApp>>,
  cookie: string,
  fields: Record<string, string>,
): Promise<{ statusCode: number; location: string }> {
  const page = await app.inject({ method: 'GET', url: '/dashboard/providers', headers: { cookie } });
  const response = await app.inject({
    method: 'POST',
    url: '/dashboard/providers',
    headers: { cookie, 'content-type': 'application/x-www-form-urlencoded' },
    payload: new URLSearchParams({
      _csrf: csrfFrom(page.body),
      revision: revisionFrom(page.body),
      ...fields,
    }).toString(),
  });
  return { statusCode: response.statusCode, location: String(response.headers['location'] ?? '') };
}

function signedHeaders(config: AppConfig, path: string, payload: Record<string, string>): Record<string, string> {
  return {
    'content-type': 'application/x-www-form-urlencoded',
    'x-twilio-signature': twilio.getExpectedTwilioSignature(
      config.twilioAuthToken,
      `${config.publicBaseUrl}${path}`,
      payload,
    ),
  };
}

describe('secret sealing', () => {
  it('round-trips a value and hides the plaintext', () => {
    const box = new SecretBox(secretsKey);
    const sealed = box.seal('sk-live-example');

    expect(sealed).not.toContain('sk-live-example');
    expect(sealed.startsWith('v1.')).toBe(true);
    expect(box.open(sealed)).toBe('sk-live-example');
    // Every sealing uses a fresh IV, so the same key never produces the same ciphertext.
    expect(box.seal('sk-live-example')).not.toBe(sealed);
  });

  it('refuses a value sealed under another key, or one that has been altered', () => {
    const sealed = new SecretBox(secretsKey).seal('sk-live-example');

    expect(() => new SecretBox('a-completely-different-key').open(sealed)).toThrow();

    const [version, iv, tag, ciphertext] = sealed.split('.');
    const flipped = `${ciphertext!.slice(0, -2)}${ciphertext!.endsWith('AA') ? 'BB' : 'AA'}`;
    expect(() => new SecretBox(secretsKey).open([version, iv, tag, flipped].join('.'))).toThrow();
  });
});

describe('platform settings resolution', () => {
  it('recovers the values the running configuration was built from', () => {
    const values = platformValuesFromConfig(envConfig, {});

    expect(values['VOICE_PROVIDER']).toBe('openai');
    expect(values['OPENAI_API_KEY']).toBe('test-openai-key');
    expect(values['ELEVENLABS_API_KEY']).toBeUndefined();
  });

  it('merges stored values over the environment and reverts when they are removed', async () => {
    const store = new MemoryStore();
    const settings = new PlatformSettings({
      store,
      config: envConfig,
      secretBox: new SecretBox(secretsKey),
      logger: silentLogger,
      environment: {},
    });
    await settings.load();
    expect(settings.providers().openai?.apiKey).toBe('test-openai-key');

    await settings.save({ values: { OPENAI_API_KEY: 'sk-from-the-dashboard' } });
    expect(settings.providers().openai?.apiKey).toBe('sk-from-the-dashboard');
    // Stored sealed, never in the clear.
    expect(store.platformSettings[0]?.secret).toBe(true);
    expect(store.platformSettings[0]?.value).not.toContain('sk-from-the-dashboard');

    await settings.save({ values: {}, clear: ['OPENAI_API_KEY'] });
    expect(settings.providers().openai?.apiKey).toBe('test-openai-key');
    expect(store.platformSettings).toHaveLength(0);
  });

  it('keeps a stored credential when the field is submitted blank', async () => {
    const store = new MemoryStore();
    const settings = new PlatformSettings({
      store,
      config: bareConfig,
      secretBox: new SecretBox(secretsKey),
      logger: silentLogger,
      environment: {},
    });
    await settings.load();

    await settings.save({ values: { OPENAI_API_KEY: 'sk-typed-once' } });
    const changed = await settings.save({ values: { OPENAI_API_KEY: '   ' } });

    expect(changed).toEqual([]);
    expect(settings.providers().openai?.apiKey).toBe('sk-typed-once');
  });

  it('rejects a credential when the deployment has no encryption key', async () => {
    const settings = new PlatformSettings({
      store: new MemoryStore(),
      config: bareConfig,
      secretBox: null,
      logger: silentLogger,
      environment: {},
    });
    await settings.load();

    await expect(settings.save({ values: { OPENAI_API_KEY: 'sk-nowhere-to-put-this' } })).rejects.toThrow(
      /SECRETS_KEY/,
    );
    expect(settings.secretsEditable()).toBe(false);
    // Non-secret settings are still manageable without a key.
    expect(await settings.save({ values: { CARTESIA_TTS_MODEL: 'sonic-3' } })).toEqual(['CARTESIA_TTS_MODEL']);
  });

  it('validates the values it is given', async () => {
    const settings = new PlatformSettings({
      store: new MemoryStore(),
      config: bareConfig,
      secretBox: new SecretBox(secretsKey),
      logger: silentLogger,
      environment: {},
    });
    await settings.load();

    await expect(settings.save({ values: { VOICE_PROVIDER: 'azure' } })).rejects.toThrow(/VOICE_PROVIDER/);
    await expect(settings.save({ values: { ALLOW_LIST: '0501234567' } })).rejects.toThrow(/E.164/);
  });

  it('does not write an override for a field that was submitted unchanged', async () => {
    const store = new MemoryStore();
    const settings = new PlatformSettings({
      store,
      config: envConfig,
      secretBox: new SecretBox(secretsKey),
      logger: silentLogger,
      environment: {},
    });
    await settings.load();

    // Exactly what the pre-filled form posts back when nothing was typed into it.
    const changed = await settings.save({
      values: {
        VOICE_PROVIDER: 'openai',
        OPENAI_REALTIME_URL: 'wss://api.openai.com/v1/realtime',
        OPENAI_API_KEY: '',
      },
    });

    expect(changed).toEqual([]);
    expect(store.platformSettings).toHaveLength(0);
  });

  it('keeps serving the environment when the settings table cannot be read', async () => {
    const store = new MemoryStore();
    store.listPlatformSettings = async () => {
      throw new Error('relation "platform_settings" does not exist');
    };
    const settings = new PlatformSettings({
      store,
      config: envConfig,
      secretBox: new SecretBox(secretsKey),
      logger: silentLogger,
      environment: {},
    });

    await settings.load();
    expect(settings.providers().openai?.apiKey).toBe('test-openai-key');
  });

  it('falls back to the environment when a stored secret cannot be decrypted', async () => {
    const store = new MemoryStore();
    await store.upsertPlatformSetting({
      key: 'OPENAI_API_KEY',
      value: new SecretBox('the-key-this-deployment-no-longer-has').seal('sk-unreachable'),
      secret: true,
    });
    const settings = new PlatformSettings({
      store,
      config: envConfig,
      secretBox: new SecretBox(secretsKey),
      logger: silentLogger,
      environment: {},
    });
    await settings.load();

    expect(settings.providers().openai?.apiKey).toBe('test-openai-key');
    const view = settings.view().find((setting) => setting.key === 'OPENAI_API_KEY');
    expect(view?.unreadable).toBe(true);
  });
});

describe('propagating a change to a running server', () => {
  /** A second process against the same database: what a replica, or a direct edit, looks like. */
  const secondInstance = (store: MemoryStore): PlatformSettings =>
    new PlatformSettings({
      store,
      config: envConfig,
      secretBox: new SecretBox(secretsKey),
      logger: silentLogger,
      environment: {},
    });

  it('picks up a change made by another instance once the snapshot ages out', async () => {
    const store = new MemoryStore();
    const dashboard = secondInstance(store);
    const caller = secondInstance(store);
    await Promise.all([dashboard.load(), caller.load()]);

    await dashboard.save({ values: { OPENAI_API_KEY: 'sk-saved-elsewhere' } });

    // The instance that did not handle the save is still on its own snapshot...
    expect(caller.providers().openai?.apiKey).toBe('test-openai-key');
    // ...and refuses to re-read while that snapshot is inside its TTL.
    await caller.refreshIfStale();
    expect(caller.providers().openai?.apiKey).toBe('test-openai-key');

    // Once it has aged out, the next read converges.
    await caller.refreshIfStale(0);
    expect(caller.providers().openai?.apiKey).toBe('sk-saved-elsewhere');
  });

  it('collapses concurrent refreshes into a single read', async () => {
    const store = new MemoryStore();
    let reads = 0;
    const listPlatformSettings = store.listPlatformSettings.bind(store);
    store.listPlatformSettings = async () => {
      reads += 1;
      return listPlatformSettings();
    };

    const settings = secondInstance(store);
    await settings.load();
    reads = 0;

    await Promise.all(Array.from({ length: 10 }, () => settings.refreshIfStale(0)));
    expect(reads).toBe(1);

    // And a fresh snapshot is not re-read at all.
    await settings.refreshIfStale();
    expect(reads).toBe(1);
  });

  it('answers a call on credentials another instance saved', async () => {
    const store = new MemoryStore();
    await withAdmin(store);
    store.agentConfigs.push(agentConfig(firstBusinessId));

    // This server starts with no credentials, so the business cannot be served yet.
    const platform = new PlatformSettings({
      store,
      config: bareConfig,
      secretBox: new SecretBox(secretsKey),
      logger: silentLogger,
      environment: {},
      // Re-read on every check, so the test does not have to wait out a TTL.
      ttlMs: 0,
    });
    const app = await buildApp({ config: bareConfig, store, platform });
    const path = '/webhooks/twilio/voice';
    const call = async (sid: string): Promise<string> => {
      const payload = { To: firstNumber, From: secondNumber, CallSid: sid, CallStatus: 'ringing' };
      const response = await app.inject({
        method: 'POST',
        url: path,
        headers: signedHeaders(bareConfig, path, payload),
        payload: new URLSearchParams(payload).toString(),
      });
      return response.body;
    };

    expect(await call('CABEFORE')).toContain('Hello from the first business.');

    // Somebody saves the key in the dashboard of another instance.
    const elsewhere = new PlatformSettings({
      store,
      config: bareConfig,
      secretBox: new SecretBox(secretsKey),
      logger: silentLogger,
      environment: {},
    });
    await elsewhere.load();
    await elsewhere.save({ values: { OPENAI_API_KEY: 'sk-saved-on-another-instance' } });

    // The running server picks them up on the next call, with nothing restarted.
    expect(await call('CAAFTER')).toContain('<Connect>');
    await app.close();
  });

  it('saves against the current stored state, not a stale form', async () => {
    const store = new MemoryStore();
    await withAdmin(store);
    const app = await buildApp({ config: bareConfig, store });
    const cookie = await signIn(app);

    // Another instance sets the transcription model after this form was rendered.
    const page = await app.inject({ method: 'GET', url: '/dashboard/providers', headers: { cookie } });
    const elsewhere = new PlatformSettings({
      store,
      config: bareConfig,
      secretBox: new SecretBox(secretsKey),
      logger: silentLogger,
      environment: {},
    });
    await elsewhere.load();
    await elsewhere.save({ values: { OPENAI_TRANSCRIBE_MODEL: 'gpt-4o-mini-transcribe' } });

    // Submitting the stale form is refused rather than quietly reverting their change.
    const response = await app.inject({
      method: 'POST',
      url: '/dashboard/providers',
      headers: { cookie, 'content-type': 'application/x-www-form-urlencoded' },
      payload: new URLSearchParams({
        _csrf: csrfFrom(page.body),
        // The revision this page was rendered with, now superseded.
        revision: revisionFrom(page.body),
        OPENAI_API_KEY: 'sk-typed-now',
        OPENAI_REALTIME_URL: 'wss://api.openai.com/v1/realtime',
        OPENAI_TRANSCRIBE_MODEL: 'gpt-4o-transcribe',
      }).toString(),
    });

    expect(response.statusCode).toBe(303);
    expect(decodeURIComponent(String(response.headers['location']))).toContain('changed somewhere else');

    const stored = new Map(store.platformSettings.map((setting) => [setting.key, setting]));
    // Their change stands, and nothing from the stale page was written.
    expect(stored.get('OPENAI_TRANSCRIBE_MODEL')?.value).toBe('gpt-4o-mini-transcribe');
    expect(stored.has('OPENAI_API_KEY')).toBe(false);

    // Reloading the page picks up the newer revision, and the same save then works.
    const reloaded = await app.inject({ method: 'GET', url: '/dashboard/providers', headers: { cookie } });
    const retry = await app.inject({
      method: 'POST',
      url: '/dashboard/providers',
      headers: { cookie, 'content-type': 'application/x-www-form-urlencoded' },
      payload: new URLSearchParams({
        _csrf: csrfFrom(reloaded.body),
        revision: revisionFrom(reloaded.body),
        OPENAI_API_KEY: 'sk-typed-now',
      }).toString(),
    });
    expect(decodeURIComponent(String(retry.headers['location']))).toContain('notice=');
    await app.close();
  });
});

describe('the providers dashboard', () => {
  let store: MemoryStore;

  beforeEach(async () => {
    store = new MemoryStore();
    await withAdmin(store);
  });

  it('brings a provider into service without a restart', async () => {
    const app = await buildApp({ config: bareConfig, store });
    const cookie = await signIn(app);

    const before = await app.inject({ method: 'GET', url: '/api/providers', headers: { cookie } });
    expect(JSON.parse(before.body).data.find((p: { id: string }) => p.id === 'openai').configured).toBe(false);

    const saved = await saveSettings(app, cookie, { OPENAI_API_KEY: 'sk-entered-in-the-browser' });
    expect(saved.statusCode).toBe(303);
    expect(saved.location).toContain('notice=');

    const after = await app.inject({ method: 'GET', url: '/api/providers', headers: { cookie } });
    expect(JSON.parse(after.body).data.find((p: { id: string }) => p.id === 'openai').configured).toBe(true);

    // The agent could not be enabled a moment ago; now the same request succeeds.
    const enable = await app.inject({
      method: 'PUT',
      url: `/api/businesses/${firstBusinessId}/agent`,
      headers: { cookie, 'content-type': 'application/json' },
      payload: {
        enabled: true,
        language: 'he-IL',
        greeting: 'שלום',
        instructions: 'Answer briefly.',
        voiceProvider: 'openai',
        voice: 'marin',
        realtimeModel: 'gpt-realtime-2.1',
      },
    });
    expect(enable.statusCode).toBe(200);
    await app.close();
  });

  it('never renders a stored credential back to the browser', async () => {
    const app = await buildApp({ config: bareConfig, store });
    const cookie = await signIn(app);
    await saveSettings(app, cookie, { OPENAI_API_KEY: 'sk-never-show-me' });

    for (const url of ['/dashboard/providers', '/dashboard/settings']) {
      const page = await app.inject({ method: 'GET', url, headers: { cookie } });
      expect(page.body).not.toContain('sk-never-show-me');
    }
    // Nor in the sealed form: only the fact that a value exists is rendered.
    const page = await app.inject({ method: 'GET', url: '/dashboard/providers', headers: { cookie } });
    expect(page.body).not.toContain(store.platformSettings[0]!.value);
    expect(page.body).toContain('Configured');
    await app.close();
  });

  it('records the audit trail by key name only', async () => {
    const app = await buildApp({ config: bareConfig, store });
    const cookie = await signIn(app);
    await saveSettings(app, cookie, { OPENAI_API_KEY: 'sk-audited', OPENAI_TRANSCRIBE_MODEL: 'gpt-4o-transcribe' });

    const event = store.auditEvents.find((entry) => entry.action === 'platform.settings_updated');
    expect(event).toBeDefined();
    expect(event!.summary).toContain('OPENAI_API_KEY');
    expect(JSON.stringify(event)).not.toContain('sk-audited');
    await app.close();
  });

  it('changes the provider new agents start on', async () => {
    const app = await buildApp({ config: { ...bareConfig, secretsKey }, store });
    const cookie = await signIn(app);
    await saveSettings(app, cookie, { VOICE_PROVIDER: 'cartesia' });

    const created = await app.inject({
      method: 'POST',
      url: '/api/businesses',
      headers: { cookie, 'content-type': 'application/json' },
      payload: { name: 'Third', phoneNumber: '+15550001111', greeting: 'Hi', active: true },
    });

    expect(created.statusCode).toBe(201);
    expect(JSON.parse(created.body).data.agent.voiceProvider).toBe('cartesia');
    await app.close();
  });

  it('gates callers on an allowlist entered in the dashboard', async () => {
    store.agentConfigs.push(agentConfig(firstBusinessId));
    const app = await buildApp({ config: envConfig, store });
    const cookie = await signIn(app);
    await saveSettings(app, cookie, { ALLOW_LIST: secondNumber });

    const path = '/webhooks/twilio/voice';
    const call = async (from: string): Promise<string> => {
      const payload = { To: firstNumber, From: from, CallSid: `CA${from}`, CallStatus: 'ringing' };
      const response = await app.inject({
        method: 'POST',
        url: path,
        headers: signedHeaders(envConfig, path, payload),
        payload: new URLSearchParams(payload).toString(),
      });
      return response.body;
    };

    expect(await call(secondNumber)).toContain('<Connect>');
    const rejected = await call('+15559999999');
    expect(rejected).toContain('not available for testing');
    expect(rejected).not.toContain('<Connect>');
    await app.close();
  });

  it('reports a rejected value without saving anything', async () => {
    const app = await buildApp({ config: bareConfig, store });
    const cookie = await signIn(app);

    const saved = await saveSettings(app, cookie, { OPENAI_API_KEY: 'sk-valid', ALLOW_LIST: 'not-a-number' });

    expect(saved.statusCode).toBe(303);
    expect(decodeURIComponent(saved.location)).toContain('E.164');
    // The whole submission is rejected: nothing is written when one field is invalid.
    expect(store.platformSettings).toHaveLength(0);
    await app.close();
  });

  it('refuses a settings post without the session CSRF token', async () => {
    const app = await buildApp({ config: bareConfig, store });
    const cookie = await signIn(app);

    const response = await app.inject({
      method: 'POST',
      url: '/dashboard/providers',
      headers: { cookie, 'content-type': 'application/x-www-form-urlencoded' },
      payload: new URLSearchParams({ OPENAI_API_KEY: 'sk-forged' }).toString(),
    });

    expect(response.statusCode).toBe(400);
    expect(store.platformSettings).toHaveLength(0);
    await app.close();
  });

  it('keeps the settings behind authentication', async () => {
    const app = await buildApp({ config: bareConfig, store });

    const page = await app.inject({ method: 'GET', url: '/dashboard/providers' });
    expect(page.statusCode).toBe(303);
    const post = await app.inject({
      method: 'POST',
      url: '/dashboard/providers',
      headers: { 'content-type': 'application/x-www-form-urlencoded' },
      payload: new URLSearchParams({ OPENAI_API_KEY: 'sk-anonymous' }).toString(),
    });
    expect(post.statusCode).toBe(303);
    expect(store.platformSettings).toHaveLength(0);
    await app.close();
  });
});

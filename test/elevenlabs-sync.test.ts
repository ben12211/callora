import { beforeEach, describe, expect, it } from 'vitest';
import { buildApp } from '../src/app.js';
import { hashPassword } from '../src/auth/passwords.js';
import type { AppConfig } from '../src/config.js';
import {
  agentConfigurationFor,
  mergeAgentConfiguration,
  pushAgentConfiguration,
} from '../src/realtime/elevenlabs-management.js';
import { MemoryStore, agentConfig, firstBusinessId, testConfig } from './support/memory-store.js';

const password = 'correct horse battery staple';
const OWN_AGENT = 'agent_owned_by_this_business';

const config: AppConfig = {
  ...testConfig,
  providers: {
    ...testConfig.providers,
    elevenlabs: {
      apiKey: 'test-elevenlabs-key',
      agentId: 'agent_platform_wide',
      apiBaseUrl: 'https://api.elevenlabs.io',
    },
  },
};

interface RecordedRequest {
  url: string;
  method: string;
  body?: unknown;
  apiKey?: string;
}

/**
 * Stands in for the ElevenLabs agent API: answers the read with a configuration that has
 * fields this build knows nothing about, so the merge can be checked for what it keeps.
 */
function fakeElevenLabs(options: { patchStatus?: number; getStatus?: number } = {}): {
  fetchImpl: typeof fetch;
  requests: RecordedRequest[];
} {
  const requests: RecordedRequest[] = [];
  const existing = {
    conversation_config: {
      agent: {
        prompt: { prompt: 'the old prompt', llm: 'old-model', tool_ids: ['tool_1'] },
        first_message: 'the old greeting',
        language: 'en',
      },
      tts: { voice_id: 'old-voice', stability: 0.7 },
      turn: { turn_timeout: 9 },
    },
    name: 'Some agent',
  };

  const fetchImpl = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const method = init?.method ?? 'GET';
    const headers = (init?.headers ?? {}) as Record<string, string>;
    requests.push({
      url: String(input),
      method,
      apiKey: headers['xi-api-key'],
      ...(typeof init?.body === 'string' ? { body: JSON.parse(init.body) } : {}),
    });

    if (method === 'GET') {
      const status = options.getStatus ?? 200;
      return {
        ok: status < 400,
        status,
        json: async () => existing,
      } as Response;
    }
    const status = options.patchStatus ?? 200;
    return { ok: status < 400, status, json: async () => ({}) } as Response;
  }) as unknown as typeof fetch;

  return { fetchImpl, requests };
}

describe('writing the configuration into the ElevenLabs agent', () => {
  it('replaces only the fields Callora owns', () => {
    const merged = mergeAgentConfiguration(
      {
        agent: {
          prompt: { prompt: 'old', llm: 'old-model', tool_ids: ['tool_1'] },
          first_message: 'old greeting',
          language: 'en',
        },
        tts: { voice_id: 'old-voice', stability: 0.7 },
        turn: { turn_timeout: 9 },
      },
      {
        prompt: 'the new prompt',
        firstMessage: 'the new greeting',
        language: 'he',
        llmModel: 'new-model',
        voiceId: 'new-voice',
      },
    );

    const agent = merged['agent'] as Record<string, Record<string, unknown>>;
    expect(agent['prompt']!['prompt']).toBe('the new prompt');
    expect(agent['prompt']!['llm']).toBe('new-model');
    expect(agent['first_message']).toBe('the new greeting');
    expect(agent['language']).toBe('he');
    expect((merged['tts'] as Record<string, unknown>)['voice_id']).toBe('new-voice');

    // Anything this build does not model survives the round trip.
    expect(agent['prompt']!['tool_ids']).toEqual(['tool_1']);
    expect((merged['tts'] as Record<string, unknown>)['stability']).toBe(0.7);
    expect(merged['turn']).toEqual({ turn_timeout: 9 });
  });

  it('leaves a field alone when the business has not set it', () => {
    const merged = mergeAgentConfiguration(
      { tts: { voice_id: 'keep-me' } },
      { prompt: 'p', firstMessage: 'g' },
    );
    expect((merged['tts'] as Record<string, unknown>)['voice_id']).toBe('keep-me');
    expect((merged['agent'] as Record<string, Record<string, unknown>>)['prompt']!['llm']).toBeUndefined();
  });

  it('reads the agent and writes it back, without the key leaving the header', async () => {
    const { fetchImpl, requests } = fakeElevenLabs();
    const agent = { ...agentConfig(firstBusinessId), voiceProvider: 'elevenlabs' as const, voice: 'v1' };

    const result = await pushAgentConfiguration({
      apiKey: 'test-elevenlabs-key',
      baseUrl: 'https://api.elevenlabs.io',
      agentId: OWN_AGENT,
      update: agentConfigurationFor(agent),
      fetchImpl,
    });

    expect(result.ok).toBe(true);
    expect(requests.map((request) => request.method)).toEqual(['GET', 'PATCH']);
    expect(requests[0]!.url).toBe(`https://api.elevenlabs.io/v1/convai/agents/${OWN_AGENT}`);
    expect(requests[0]!.apiKey).toBe('test-elevenlabs-key');
    // The key travels in the header only; the URL never carries it.
    expect(requests.every((request) => !request.url.includes('test-elevenlabs-key'))).toBe(true);

    const patched = (requests[1]!.body as Record<string, Record<string, Record<string, unknown>>>)[
      'conversation_config'
    ]!;
    expect((patched['agent']!['prompt'] as Record<string, unknown>)['prompt']).toContain(agent.instructions);
    expect(patched['agent']!['first_message']).toBe(agent.greeting);
    expect(patched['agent']!['language']).toBe('he');
  });

  it('reports a rejection instead of throwing', async () => {
    const { fetchImpl } = fakeElevenLabs({ patchStatus: 422 });
    const result = await pushAgentConfiguration({
      apiKey: 'k',
      baseUrl: 'https://api.elevenlabs.io',
      agentId: OWN_AGENT,
      update: { prompt: 'p', firstMessage: 'g' },
      fetchImpl,
    });

    expect(result.ok).toBe(false);
    expect(result).toMatchObject({ status: 422 });

    const missing = await pushAgentConfiguration({
      apiKey: 'k',
      baseUrl: 'https://api.elevenlabs.io',
      agentId: 'agent_that_does_not_exist',
      update: { prompt: 'p', firstMessage: 'g' },
      fetchImpl: fakeElevenLabs({ getStatus: 404 }).fetchImpl,
    });
    expect(missing.ok).toBe(false);
    expect(missing).toMatchObject({ status: 404 });
  });

  it('survives a provider that cannot be reached at all', async () => {
    const result = await pushAgentConfiguration({
      apiKey: 'k',
      baseUrl: 'https://api.elevenlabs.io',
      agentId: OWN_AGENT,
      update: { prompt: 'p', firstMessage: 'g' },
      fetchImpl: (async () => {
        throw new Error('getaddrinfo ENOTFOUND');
      }) as unknown as typeof fetch,
    });
    expect(result.ok).toBe(false);
    expect(result).toMatchObject({ message: expect.stringContaining('Could not reach ElevenLabs') });
  });
});

describe('saving in the dashboard pushes to ElevenLabs', () => {
  let store: MemoryStore;

  beforeEach(async () => {
    store = new MemoryStore();
    await store.createAdminUser({
      email: 'admin@callora.test',
      name: 'Admin',
      passwordHash: await hashPassword(password),
    });
  });

  const signIn = async (app: Awaited<ReturnType<typeof buildApp>>): Promise<string> => {
    const response = await app.inject({
      method: 'POST',
      url: '/dashboard/login',
      headers: { 'content-type': 'application/x-www-form-urlencoded' },
      payload: new URLSearchParams({ email: 'admin@callora.test', password }).toString(),
    });
    const raw = response.headers['set-cookie'];
    return (Array.isArray(raw) ? raw[0] : raw)!.split(';')[0]!;
  };

  const saveAgent = async (
    app: Awaited<ReturnType<typeof buildApp>>,
    cookie: string,
    fields: Record<string, string>,
  ): Promise<string> => {
    const page = await app.inject({
      method: 'GET',
      url: `/dashboard/businesses/${firstBusinessId}`,
      headers: { cookie },
    });
    const csrf = /name="_csrf" value="([^"]+)"/.exec(page.body)![1]!;
    const response = await app.inject({
      method: 'POST',
      url: `/dashboard/businesses/${firstBusinessId}/agent`,
      headers: { cookie, 'content-type': 'application/x-www-form-urlencoded' },
      payload: new URLSearchParams({
        _csrf: csrf,
        enabled: 'on',
        language: 'he-IL',
        greeting: 'שלום מהטופס',
        instructions: 'MARKER instructions from the form',
        voiceProvider: 'elevenlabs',
        voice: 'voice-from-the-form',
        realtimeModel: 'model-from-the-form',
        ...fields,
      }).toString(),
    });
    expect(response.statusCode).toBe(303);
    return decodeURIComponent(String(response.headers['location']));
  };

  it('writes what the form saved into the business own agent', async () => {
    const { fetchImpl, requests } = fakeElevenLabs();
    const app = await buildApp({ config, store, fetchImpl });
    const cookie = await signIn(app);

    const location = await saveAgent(app, cookie, { elevenLabsAgentId: OWN_AGENT });

    expect(location).toContain('Pushed to ElevenLabs');
    expect(requests.map((request) => request.method)).toEqual(['GET', 'PATCH']);
    const patched = (requests[1]!.body as Record<string, Record<string, Record<string, unknown>>>)[
      'conversation_config'
    ]!;
    expect((patched['agent']!['prompt'] as Record<string, unknown>)['prompt']).toContain(
      'MARKER instructions from the form',
    );
    // The Model field finally means something on this provider.
    expect((patched['agent']!['prompt'] as Record<string, unknown>)['llm']).toBe('model-from-the-form');
    expect(patched['agent']!['first_message']).toBe('שלום מהטופס');
    expect((patched['tts'] as Record<string, unknown>)['voice_id']).toBe('voice-from-the-form');
    await app.close();
  });

  it('refuses to write into the shared platform agent', async () => {
    const { fetchImpl, requests } = fakeElevenLabs();
    const app = await buildApp({ config, store, fetchImpl });
    const cookie = await signIn(app);

    const location = await saveAgent(app, cookie, { elevenLabsAgentId: '' });

    // Saved, but nothing was pushed: one tenant must not overwrite another's prompt.
    expect(location).toContain('Agent configuration saved');
    expect(location).toContain('no agent of its own');
    expect(requests).toHaveLength(0);
    expect((await store.getAgentConfig(firstBusinessId))!.instructions).toBe(
      'MARKER instructions from the form',
    );
    await app.close();
  });

  it('still saves when ElevenLabs rejects the update, and says so', async () => {
    const { fetchImpl } = fakeElevenLabs({ patchStatus: 500 });
    const app = await buildApp({ config, store, fetchImpl });
    const cookie = await signIn(app);

    const location = await saveAgent(app, cookie, { elevenLabsAgentId: OWN_AGENT });

    expect(location).toContain('Agent configuration saved');
    expect(location).toContain('ElevenLabs still has the previous configuration');
    // The row in Callora is correct regardless of what the provider did.
    const stored = await store.getAgentConfig(firstBusinessId);
    expect(stored!.elevenLabsAgentId).toBe(OWN_AGENT);
    expect(stored!.realtimeModel).toBe('model-from-the-form');

    const failure = store.auditEvents.find((event) => event.action === 'agent.pushed_to_provider');
    expect(failure).toBeDefined();
    expect(failure!.details['ok']).toBe(false);
    await app.close();
  });

  it('does not touch ElevenLabs for a business on another provider', async () => {
    const { fetchImpl, requests } = fakeElevenLabs();
    const app = await buildApp({ config, store, fetchImpl });
    const cookie = await signIn(app);

    const location = await saveAgent(app, cookie, { voiceProvider: 'openai', voice: 'marin' });

    expect(location).toContain('Agent configuration saved');
    expect(requests).toHaveLength(0);
    await app.close();
  });

  it('routes the call to the agent the business owns', async () => {
    store.agentConfigs.push({
      ...agentConfig(firstBusinessId),
      voiceProvider: 'elevenlabs',
      elevenLabsAgentId: OWN_AGENT,
    });
    const stored = await store.getAgentConfig(firstBusinessId);

    // What the media layer resolves before opening the socket.
    expect(stored!.elevenLabsAgentId.trim() || config.providers.elevenlabs!.agentId).toBe(OWN_AGENT);

    const shared = { ...stored!, elevenLabsAgentId: '' };
    expect(shared.elevenLabsAgentId.trim() || config.providers.elevenlabs!.agentId).toBe('agent_platform_wide');
  });
});

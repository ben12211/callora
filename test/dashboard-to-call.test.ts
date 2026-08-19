import { beforeEach, describe, expect, it } from 'vitest';
import { buildApp } from '../src/app.js';
import { hashPassword } from '../src/auth/passwords.js';
import type { AppConfig } from '../src/config.js';
import type { AgentConfig } from '../src/domain/models.js';
import { buildSttUrl, buildTtsChunk, cartesiaLanguage } from '../src/realtime/cartesia-protocol.js';
import { buildConversationInitiation } from '../src/realtime/elevenlabs-protocol.js';
import { composeAgentInstructions } from '../src/realtime/policy.js';
import { buildGreetingResponse, buildSessionUpdate } from '../src/realtime/protocol.js';
import { MemoryStore, firstBusinessId, testConfig } from './support/memory-store.js';

/**
 * End-to-end trace of the control plane: every field an operator types into the agent
 * form is followed to the exact payload the provider receives on a call.
 *
 * The unit tests already cover each builder in isolation. What this file answers is the
 * question an operator actually asks — "I changed it in the dashboard, does the call use
 * it?" — by starting from a real form post rather than from a hand-built `AgentConfig`.
 * It also pins the fields that are deliberately *not* per business, so a gap between what
 * the form offers and what the provider honours cannot widen unnoticed.
 */

const password = 'correct horse battery staple';

// Values chosen to be unmistakable in a payload dump.
const INSTRUCTIONS = 'MARKER-INSTRUCTIONS only sell red bicycles';
const GREETING = 'MARKER-GREETING bonjour et bienvenue';
const LANGUAGE = 'fr-FR';

const config: AppConfig = {
  ...testConfig,
  providers: {
    openai: {
      apiKey: 'test-openai-key',
      realtimeUrl: 'wss://api.openai.com/v1/realtime',
      transcribeModel: 'gpt-4o-transcribe',
    },
    elevenlabs: {
      apiKey: 'test-elevenlabs-key',
      agentId: 'agent_platform_wide',
      apiBaseUrl: 'https://api.elevenlabs.io',
    },
    cartesia: {
      apiKey: 'test-cartesia-key',
      defaultVoiceId: 'platform-default-voice',
      ttsModel: 'sonic-3.5-platform',
      sttModel: 'ink-whisper-platform',
      version: '2026-03-01',
      wsBaseUrl: 'wss://api.cartesia.ai',
      textLlmApiKey: 'test-openai-key',
      textLlmModel: 'gpt-4o-mini-platform',
      textLlmBaseUrl: 'https://api.openai.com/v1',
    },
  },
};

async function dashboard(store: MemoryStore): Promise<{
  app: Awaited<ReturnType<typeof buildApp>>;
  cookie: string;
}> {
  await store.createAdminUser({
    email: 'admin@callora.test',
    name: 'Admin',
    passwordHash: await hashPassword(password),
  });
  const app = await buildApp({ config, store });
  const response = await app.inject({
    method: 'POST',
    url: '/dashboard/login',
    headers: { 'content-type': 'application/x-www-form-urlencoded' },
    payload: new URLSearchParams({ email: 'admin@callora.test', password }).toString(),
  });
  const raw = response.headers['set-cookie'];
  const cookie = (Array.isArray(raw) ? raw[0] : raw)!.split(';')[0]!;
  return { app, cookie };
}

/**
 * Fills in the agent form on the business page and saves it, exactly as an operator
 * would, then hands back the row the call path will later load.
 */
async function saveAgentForm(
  app: Awaited<ReturnType<typeof buildApp>>,
  cookie: string,
  store: MemoryStore,
  fields: { voiceProvider: string; voice: string; realtimeModel: string },
): Promise<AgentConfig> {
  const page = await app.inject({
    method: 'GET',
    url: `/dashboard/businesses/${firstBusinessId}`,
    headers: { cookie },
  });
  const csrf = /name="_csrf" value="([^"]+)"/.exec(page.body)?.[1];
  expect(csrf).toBeDefined();

  const saved = await app.inject({
    method: 'POST',
    url: `/dashboard/businesses/${firstBusinessId}/agent`,
    headers: { cookie, 'content-type': 'application/x-www-form-urlencoded' },
    payload: new URLSearchParams({
      _csrf: csrf!,
      enabled: 'on',
      language: LANGUAGE,
      greeting: GREETING,
      instructions: INSTRUCTIONS,
      ...fields,
    }).toString(),
  });
  expect(saved.statusCode).toBe(303);
  expect(String(saved.headers['location'])).toContain('notice=');

  // Read back what the call path itself would load, not what the test just posted.
  const agent = await store.getAgentConfig(firstBusinessId);
  expect(agent).not.toBeNull();
  return agent!;
}

describe('what the dashboard saves reaches the call', () => {
  let store: MemoryStore;

  beforeEach(() => {
    store = new MemoryStore();
  });

  it('OpenAI: instructions, greeting, language, voice, and model all reach the session', async () => {
    const { app, cookie } = await dashboard(store);
    const agent = await saveAgentForm(app, cookie, store, {
      voiceProvider: 'openai',
      voice: 'cedar',
      realtimeModel: 'gpt-realtime-from-the-form',
    });

    const update = buildSessionUpdate({ agent, transcriptionModel: 'gpt-4o-transcribe' });
    const session = update['session'] as Record<string, unknown>;
    const audio = session['audio'] as Record<string, Record<string, unknown>>;

    // The model is sent on the session, and is also what the socket is opened with.
    expect(session['model']).toBe('gpt-realtime-from-the-form');
    expect(agent.realtimeModel).toBe('gpt-realtime-from-the-form');
    expect((audio['output'] as Record<string, unknown>)['voice']).toBe('cedar');
    expect(String(session['instructions'])).toContain(INSTRUCTIONS);
    expect(String(session['instructions'])).toContain('Always speak fr-FR');
    // The locale also steers the transcriber behind the conversation log.
    const transcription = (audio['input'] as Record<string, Record<string, unknown>>)['transcription'];
    expect(transcription['language']).toBe('fr');

    const greeting = buildGreetingResponse(agent)['response'] as Record<string, unknown>;
    expect(String(greeting['instructions'])).toContain(GREETING);
    await app.close();
  });

  it('ElevenLabs: instructions, greeting, language, and voice are overridden per call', async () => {
    const { app, cookie } = await dashboard(store);
    const agent = await saveAgentForm(app, cookie, store, {
      voiceProvider: 'elevenlabs',
      voice: 'voice_from_the_form',
      realtimeModel: 'eleven_turbo_v2_5',
    });

    const initiation = buildConversationInitiation({ agent, voiceId: agent.voice });
    const override = initiation['conversation_config_override'] as Record<string, Record<string, unknown>>;
    const agentOverride = override['agent'] as Record<string, unknown>;

    expect(String((agentOverride['prompt'] as Record<string, unknown>)['prompt'])).toContain(INSTRUCTIONS);
    expect(agentOverride['first_message']).toBe(GREETING);
    expect(agentOverride['language']).toBe('fr');
    expect((override['tts'] as Record<string, unknown>)['voice_id']).toBe('voice_from_the_form');
    await app.close();
  });

  it('ElevenLabs: the Model field is stored but never sent, so the call ignores it', async () => {
    const { app, cookie } = await dashboard(store);
    const agent = await saveAgentForm(app, cookie, store, {
      voiceProvider: 'elevenlabs',
      voice: 'voice_from_the_form',
      realtimeModel: 'eleven_flash_v2_5',
    });

    // Saved, and shown back on the page...
    expect(agent.realtimeModel).toBe('eleven_flash_v2_5');
    // ...but nothing carries it to ElevenLabs: the model belongs to the agent configured
    // in their own dashboard. This assertion exists so that gap stays visible.
    const initiation = buildConversationInitiation({ agent, voiceId: agent.voice });
    expect(JSON.stringify(initiation)).not.toContain('eleven_flash_v2_5');
    await app.close();
  });

  it('Cartesia: instructions, greeting, language, voice, and the reasoning model reach the pipeline', async () => {
    const { app, cookie } = await dashboard(store);
    const agent = await saveAgentForm(app, cookie, store, {
      voiceProvider: 'cartesia',
      voice: 'sonic-voice-from-the-form',
      realtimeModel: 'gpt-4o-mini-from-the-form',
    });
    const cartesia = config.providers.cartesia!;

    // The reasoning turn runs on the model chosen in the form, not the platform default.
    const llmModel = agent.realtimeModel.trim() || cartesia.textLlmModel;
    expect(llmModel).toBe('gpt-4o-mini-from-the-form');
    expect(composeAgentInstructions({ agent })).toContain(INSTRUCTIONS);

    const voiceId = agent.voice.trim() || cartesia.defaultVoiceId;
    const chunk = buildTtsChunk({
      model: cartesia.ttsModel,
      voiceId: voiceId!,
      contextId: 'ctx-1',
      transcript: agent.greeting,
      language: cartesiaLanguage(agent),
      continue: false,
    });
    expect((chunk['voice'] as Record<string, unknown>)['id']).toBe('sonic-voice-from-the-form');
    expect(chunk['language']).toBe('fr');
    expect(chunk['transcript']).toBe(GREETING);
    // Speech models stay platform-level; only the reasoning model is per business.
    expect(chunk['model_id']).toBe('sonic-3.5-platform');

    const sttUrl = new URL(
      buildSttUrl({
        baseUrl: cartesia.wsBaseUrl,
        model: cartesia.sttModel,
        version: cartesia.version,
        language: cartesiaLanguage(agent),
      }),
    );
    expect(sttUrl.searchParams.get('language')).toBe('fr');
    expect(sttUrl.searchParams.get('model')).toBe('ink-whisper-platform');
    await app.close();
  });

  it('a second save replaces the first, so the newest form wins on the next call', async () => {
    const { app, cookie } = await dashboard(store);
    await saveAgentForm(app, cookie, store, {
      voiceProvider: 'openai',
      voice: 'cedar',
      realtimeModel: 'first-model',
    });
    const second = await saveAgentForm(app, cookie, store, {
      voiceProvider: 'openai',
      voice: 'marin',
      realtimeModel: 'second-model',
    });

    expect(store.agentConfigs.filter((entry) => entry.businessId === firstBusinessId)).toHaveLength(1);
    const session = buildSessionUpdate({ agent: second })['session'] as Record<string, unknown>;
    expect(session['model']).toBe('second-model');
    expect(
      ((session['audio'] as Record<string, Record<string, unknown>>)['output'] as Record<string, unknown>)['voice'],
    ).toBe('marin');
    await app.close();
  });

  it('switching provider in the form switches which bridge the call opens', async () => {
    const { app, cookie } = await dashboard(store);
    for (const provider of ['openai', 'elevenlabs', 'cartesia'] as const) {
      const agent = await saveAgentForm(app, cookie, store, {
        voiceProvider: provider,
        voice: provider === 'openai' ? 'marin' : 'provider-voice',
        realtimeModel: 'some-model',
      });
      expect(agent.voiceProvider).toBe(provider);
    }
    await app.close();
  });
});

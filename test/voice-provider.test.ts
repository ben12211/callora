import { describe, expect, it } from 'vitest';
import { loadConfig } from '../src/config.js';
import { ElevenLabsBridge } from '../src/realtime/elevenlabs-bridge.js';
import { fetchSignedUrl } from '../src/realtime/elevenlabs-connection.js';
import {
  ELEVENLABS_AUDIO_FORMAT,
  buildClientToolResult,
  buildConversationInitiation,
  resolveConversationOverrides,
  buildPong,
  buildUserAudioChunk,
} from '../src/realtime/elevenlabs-protocol.js';
import { DEFAULT_REALTIME_PROVIDER, REALTIME_PROVIDERS } from '../src/realtime/provider.js';
import {
  FakeChannel,
  RecordingLogger,
  agent,
  businessId,
  callSid,
  flush,
  silentLogger,
  streamSid,
} from './support/realtime-harness.js';

/** Everything `loadConfig` needs that is not provider-specific. */
const baseEnv = {
  NODE_ENV: 'test',
  DATABASE_URL: 'postgresql://unused',
  TWILIO_ACCOUNT_SID: 'AC00000000000000000000000000000000',
  TWILIO_AUTH_TOKEN: 'test-auth-token',
  PUBLIC_BASE_URL: 'https://voice.example.test',
} satisfies NodeJS.ProcessEnv;

describe('voice provider selection', () => {
  it('offers the supported providers and defaults to OpenAI', () => {
    expect([...REALTIME_PROVIDERS]).toEqual(['openai', 'elevenlabs', 'cartesia']);
    expect(DEFAULT_REALTIME_PROVIDER).toBe('openai');

    const config = loadConfig({ ...baseEnv, OPENAI_API_KEY: 'sk-test' });
    expect(config.voiceProvider).toBe('openai');
  });

  it('selects OpenAI explicitly and keeps its existing settings', () => {
    const config = loadConfig({
      ...baseEnv,
      VOICE_PROVIDER: 'openai',
      OPENAI_API_KEY: 'sk-test',
      OPENAI_TRANSCRIBE_MODEL: 'gpt-4o-transcribe',
    });

    expect(config.voiceProvider).toBe('openai');
    // Narrowed by the discriminant, so these are only reachable on the OpenAI branch.
    if (config.voiceProvider !== 'openai') throw new Error('expected the OpenAI provider');
    expect(config.openaiApiKey).toBe('sk-test');
    expect(config.openaiRealtimeUrl).toBe('wss://api.openai.com/v1/realtime');
    expect(config.openaiTranscribeModel).toBe('gpt-4o-transcribe');
  });

  it('selects ElevenLabs with its own credentials', () => {
    const config = loadConfig({
      ...baseEnv,
      VOICE_PROVIDER: 'elevenlabs',
      ELEVENLABS_API_KEY: 'xi-test',
      ELEVENLABS_AGENT_ID: 'agent_123',
    });

    expect(config.voiceProvider).toBe('elevenlabs');
    if (config.voiceProvider !== 'elevenlabs') throw new Error('expected the ElevenLabs provider');
    expect(config.elevenLabsApiKey).toBe('xi-test');
    expect(config.elevenLabsAgentId).toBe('agent_123');
    expect(config.elevenLabsApiBaseUrl).toBe('https://api.elevenlabs.io');
  });

  // Compose and the deployment secret sync always define every provider variable, so
  // the unused provider's credentials arrive as empty strings rather than being absent.
  // Treating that as a validation failure would crash a valid single-provider server.
  it('treats an empty credential as absent rather than invalid', () => {
    const onElevenLabs = loadConfig({
      ...baseEnv,
      VOICE_PROVIDER: 'elevenlabs',
      ELEVENLABS_API_KEY: 'xi-test',
      ELEVENLABS_AGENT_ID: 'agent_123',
      OPENAI_API_KEY: '',
    });
    expect(onElevenLabs.voiceProvider).toBe('elevenlabs');

    const onOpenAi = loadConfig({
      ...baseEnv,
      VOICE_PROVIDER: 'openai',
      OPENAI_API_KEY: 'sk-test',
      ELEVENLABS_API_KEY: '',
      ELEVENLABS_AGENT_ID: '',
    });
    expect(onOpenAi.voiceProvider).toBe('openai');
  });

  it('falls back to the default when optional values arrive blank', () => {
    const config = loadConfig({
      ...baseEnv,
      VOICE_PROVIDER: '',
      OPENAI_API_KEY: 'sk-test',
      OPENAI_REALTIME_URL: '',
      OPENAI_TRANSCRIBE_MODEL: '',
    });

    expect(config.voiceProvider).toBe('openai');
    if (config.voiceProvider !== 'openai') throw new Error('expected the OpenAI provider');
    expect(config.openaiRealtimeUrl).toBe('wss://api.openai.com/v1/realtime');
    expect(config.openaiTranscribeModel).toBeTruthy();

    const elevenlabs = loadConfig({
      ...baseEnv,
      VOICE_PROVIDER: 'elevenlabs',
      ELEVENLABS_API_KEY: 'xi-test',
      ELEVENLABS_AGENT_ID: 'agent_123',
      ELEVENLABS_API_BASE_URL: '',
    });
    if (elevenlabs.voiceProvider !== 'elevenlabs') throw new Error('expected the ElevenLabs provider');
    expect(elevenlabs.elevenLabsApiBaseUrl).toBe('https://api.elevenlabs.io');
  });

  it('still rejects a blank credential the selected provider needs', () => {
    // Blank must mean "absent", not "valid" — the required check has to still fire.
    expect(() =>
      loadConfig({ ...baseEnv, VOICE_PROVIDER: 'elevenlabs', ELEVENLABS_API_KEY: '', ELEVENLABS_AGENT_ID: '' }),
    ).toThrow(/ELEVENLABS_API_KEY/);
    expect(() => loadConfig({ ...baseEnv, VOICE_PROVIDER: 'openai', OPENAI_API_KEY: '   ' })).toThrow(
      /OPENAI_API_KEY/,
    );
  });

  it('rejects a provider that is not openai or elevenlabs', () => {
    for (const provider of ['azure', 'ElevenLabs', 'openai ', 'eleven-labs']) {
      expect(() => loadConfig({ ...baseEnv, VOICE_PROVIDER: provider, OPENAI_API_KEY: 'sk-test' })).toThrow();
    }
    // A blank value is not a wrong provider: compose and the secret sync both write an
    // empty variable when nothing was configured, which means the default.
    expect(loadConfig({ ...baseEnv, VOICE_PROVIDER: '', OPENAI_API_KEY: 'sk-test' }).voiceProvider).toBe('openai');
  });

  it('requires only the selected provider credentials', () => {
    // OpenAI selected: its key is required, ElevenLabs credentials are not.
    expect(() => loadConfig({ ...baseEnv, VOICE_PROVIDER: 'openai' })).toThrow(/OPENAI_API_KEY/);
    expect(() =>
      loadConfig({ ...baseEnv, VOICE_PROVIDER: 'openai', OPENAI_API_KEY: 'sk-test' }),
    ).not.toThrow();

    // ElevenLabs selected: both of its values are required, the OpenAI key is not.
    expect(() =>
      loadConfig({ ...baseEnv, VOICE_PROVIDER: 'elevenlabs', ELEVENLABS_AGENT_ID: 'agent_123' }),
    ).toThrow(/ELEVENLABS_API_KEY/);
    expect(() =>
      loadConfig({ ...baseEnv, VOICE_PROVIDER: 'elevenlabs', ELEVENLABS_API_KEY: 'xi-test' }),
    ).toThrow(/ELEVENLABS_AGENT_ID/);
    expect(() =>
      loadConfig({
        ...baseEnv,
        VOICE_PROVIDER: 'elevenlabs',
        ELEVENLABS_API_KEY: 'xi-test',
        ELEVENLABS_AGENT_ID: 'agent_123',
      }),
    ).not.toThrow();
  });
});

describe('elevenlabs conversation overrides', () => {
  const initiation = buildConversationInitiation({ agent, callerNumber: '+972500000000' }) as {
    type: string;
    conversation_config_override: {
      agent: { prompt: { prompt: string }; first_message: string; language?: string };
    };
  };

  it('reuses the tenant agent config as per-call overrides', () => {
    expect(initiation.type).toBe('conversation_initiation_client_data');
    expect(initiation.conversation_config_override.agent.first_message).toBe(agent.greeting);
    // `he-IL` is narrowed to the bare language code ElevenLabs expects.
    expect(initiation.conversation_config_override.agent.language).toBe('he');
  });

  it('carries the same short phone-agent policy the OpenAI path uses', () => {
    const prompt = initiation.conversation_config_override.agent.prompt.prompt;
    expect(prompt).toMatch(/default to one short sentence/i);
    expect(prompt).toMatch(/eight to twelve words/i);
    expect(prompt).toMatch(/not a general-purpose assistant/i);
    expect(prompt).toContain('SPOKEN HEBREW:');
    expect(prompt).toContain(agent.instructions);
  });

  it('narrows every Hebrew locale spelling to the bare code ElevenLabs expects', () => {
    for (const locale of ['he-IL', 'he', 'he_IL', 'HE-il']) {
      const built = buildConversationInitiation({ agent: { ...agent, language: locale } }) as {
        conversation_config_override: { agent: { language?: string } };
      };
      expect(built.conversation_config_override.agent.language).toBe('he');
    }
  });

  it('sends the tenant greeting so ElevenLabs never uses its own first message', () => {
    const hebrewGreeting = 'שלום, הגעתם לקפה נוף. איך אפשר לעזור?';
    const built = buildConversationInitiation({
      agent: { ...agent, greeting: `  ${hebrewGreeting}  `, language: 'he-IL' },
    }) as { conversation_config_override: { agent: { first_message?: string; language?: string } } };

    expect(built.conversation_config_override.agent.first_message).toBe(hebrewGreeting);
    expect(built.conversation_config_override.agent.language).toBe('he');
  });

  it('omits first_message only when the tenant has no greeting at all', () => {
    const blank = buildConversationInitiation({ agent: { ...agent, greeting: '   ' } }) as {
      conversation_config_override: { agent: Record<string, unknown> };
    };
    expect(blank.conversation_config_override.agent).not.toHaveProperty('first_message');
  });

  it('resolves the same values it sends', () => {
    const resolved = resolveConversationOverrides({ agent });
    const built = buildConversationInitiation({ agent }) as {
      conversation_config_override: { agent: { first_message: string; language: string; prompt: { prompt: string } } };
    };

    expect(built.conversation_config_override.agent.first_message).toBe(resolved.firstMessage);
    expect(built.conversation_config_override.agent.language).toBe(resolved.language);
    expect(built.conversation_config_override.agent.prompt.prompt).toBe(resolved.prompt);
  });

  it('omits the language override when the locale is not a plain code', () => {
    const initiationWithoutLanguage = buildConversationInitiation({
      agent: { ...agent, language: 'not-a-locale-code' },
    }) as { conversation_config_override: { agent: Record<string, unknown> } };

    expect(initiationWithoutLanguage.conversation_config_override.agent).not.toHaveProperty('language');
  });

  it('builds the remaining client messages in the documented shape', () => {
    expect(buildUserAudioChunk('YQ==')).toEqual({ user_audio_chunk: 'YQ==' });
    expect(buildPong(7)).toEqual({ type: 'pong', event_id: 7 });
    expect(buildClientToolResult('tc_1', 'done')).toEqual({
      type: 'client_tool_result',
      tool_call_id: 'tc_1',
      result: 'done',
      is_error: false,
    });
  });
});

describe('elevenlabs signed url', () => {
  const options = { apiKey: 'xi-secret', agentId: 'agent_123', baseUrl: 'https://api.elevenlabs.io' };

  it('authenticates with the API key header and never puts it in the URL', async () => {
    let seenUrl = '';
    let seenHeaders: Record<string, string> = {};
    const fetchImpl = (async (url: string | URL, init?: RequestInit) => {
      seenUrl = String(url);
      seenHeaders = (init?.headers ?? {}) as Record<string, string>;
      return { ok: true, status: 200, json: async () => ({ signed_url: 'wss://signed.example/x' }) };
    }) as unknown as typeof fetch;

    await expect(fetchSignedUrl({ ...options, fetchImpl })).resolves.toBe('wss://signed.example/x');
    expect(seenUrl).toContain('agent_id=agent_123');
    expect(seenUrl).not.toContain('xi-secret');
    expect(seenHeaders['xi-api-key']).toBe('xi-secret');
  });

  it('reports a failure without echoing the key', async () => {
    const fetchImpl = (async () => ({ ok: false, status: 401, json: async () => ({}) })) as unknown as typeof fetch;

    await expect(fetchSignedUrl({ ...options, fetchImpl })).rejects.toThrow(/status 401/);
    await expect(fetchSignedUrl({ ...options, fetchImpl })).rejects.not.toThrow(/xi-secret/);
  });
});

interface StartedElevenLabsBridge {
  twilio: FakeChannel;
  elevenlabs: FakeChannel;
  bridge: ElevenLabsBridge;
}

function startElevenLabsBridge(
  overrides: Partial<ConstructorParameters<typeof ElevenLabsBridge>[0]> = {},
): StartedElevenLabsBridge {
  const twilio = new FakeChannel();
  const elevenlabs = new FakeChannel();
  const bridge = new ElevenLabsBridge({
    twilio,
    elevenlabs,
    agent,
    businessId,
    callSid,
    logger: silentLogger,
    ...overrides,
  });
  bridge.start();
  return { twilio, elevenlabs, bridge };
}

/** Twilio start plus the ElevenLabs metadata that confirms the mu-law session. */
function openElevenLabsStream(twilio: FakeChannel, elevenlabs: FakeChannel): void {
  twilio.emit({ event: 'start', streamSid, start: { streamSid, callSid } });
  elevenlabs.emit({
    type: 'conversation_initiation_metadata',
    conversation_initiation_metadata_event: {
      conversation_id: 'conv_123',
      agent_output_audio_format: ELEVENLABS_AUDIO_FORMAT,
      user_input_audio_format: ELEVENLABS_AUDIO_FORMAT,
    },
  });
}

describe('elevenlabs media bridge', () => {
  it('sends the overrides first and bridges audio both ways untranscoded', () => {
    const { twilio, elevenlabs } = startElevenLabsBridge();
    expect(elevenlabs.types()).toEqual(['conversation_initiation_client_data']);

    openElevenLabsStream(twilio, elevenlabs);

    twilio.emit({ event: 'media', media: { timestamp: '20', payload: 'Y2FsbGVy' } });
    expect(elevenlabs.sent.at(-1)).toEqual({ user_audio_chunk: 'Y2FsbGVy' });

    elevenlabs.emit({ type: 'audio', audio_event: { audio_base_64: 'YWdlbnQ=', event_id: 1 } });
    expect(twilio.sent).toEqual([
      { event: 'media', streamSid, media: { payload: 'YWdlbnQ=' } },
      { event: 'mark', streamSid, mark: { name: 'callora-assistant-audio' } },
    ]);
  });

  it('clears queued audio when ElevenLabs reports an interruption', () => {
    const { twilio, elevenlabs } = startElevenLabsBridge();
    openElevenLabsStream(twilio, elevenlabs);

    elevenlabs.emit({ type: 'audio', audio_event: { audio_base_64: 'YQ==', event_id: 1 } });
    elevenlabs.emit({ type: 'interruption', interruption_event: { event_id: 2 } });

    expect(twilio.sent.at(-1)).toEqual({ event: 'clear', streamSid });
  });

  it('ignores an interruption when nothing is queued', () => {
    const { twilio, elevenlabs } = startElevenLabsBridge();
    openElevenLabsStream(twilio, elevenlabs);

    elevenlabs.emit({ type: 'interruption', interruption_event: { event_id: 1 } });
    expect(twilio.sent).toHaveLength(0);
  });

  it('logs the language and greeting it actually sent, without the API key', () => {
    const logger = new RecordingLogger();
    const { elevenlabs } = startElevenLabsBridge({ logger });

    const line = logger.lines.find((entry) => entry.message.includes('conversation overrides'));
    expect(line?.level).toBe('info');
    expect(line?.details).toEqual(
      expect.objectContaining({ language: 'he', agentLanguage: agent.language, firstMessage: agent.greeting }),
    );

    // The logged values are the ones on the wire, not a re-derivation.
    const sent = elevenlabs.sent[0] as {
      conversation_config_override: { agent: { first_message: string; language: string } };
    };
    expect(sent.conversation_config_override.agent.language).toBe(line?.details['language']);
    expect(sent.conversation_config_override.agent.first_message).toBe(line?.details['firstMessage']);
  });

  it('warns when the business has no greeting to override with', () => {
    const logger = new RecordingLogger();
    startElevenLabsBridge({ logger, agent: { ...agent, greeting: '' } });

    expect(logger.lines.some((line) => line.level === 'warn' && /no greeting configured/i.test(line.message))).toBe(
      true,
    );
  });

  it('surfaces a rejected override instead of silently answering in the wrong language', () => {
    const logger = new RecordingLogger();
    const { twilio, elevenlabs } = startElevenLabsBridge({ logger });
    openElevenLabsStream(twilio, elevenlabs);

    elevenlabs.emit({
      type: 'error',
      error: { code: 'override_not_allowed', message: 'first_message override is not enabled' },
    });

    const error = logger.lines.find((line) => line.level === 'error');
    expect(error?.message).toMatch(/overrides are enabled/i);
    expect(error?.details).toEqual(
      expect.objectContaining({ code: 'override_not_allowed', reason: 'first_message override is not enabled' }),
    );
  });

  it('records an unhandled event type rather than dropping it silently', () => {
    const logger = new RecordingLogger();
    const { twilio, elevenlabs } = startElevenLabsBridge({ logger });
    openElevenLabsStream(twilio, elevenlabs);

    elevenlabs.emit({ type: 'agent_response_correction', whatever: 'ignored' });

    expect(
      logger.lines.some(
        (line) => line.level === 'debug' && line.details['event'] === 'agent_response_correction',
      ),
    ).toBe(true);
  });

  it('answers keepalive pings so the session stays open', () => {
    const { twilio, elevenlabs } = startElevenLabsBridge();
    openElevenLabsStream(twilio, elevenlabs);

    elevenlabs.emit({ type: 'ping', ping_event: { event_id: 42, ping_ms: 5 } });
    expect(elevenlabs.sent.at(-1)).toEqual({ type: 'pong', event_id: 42 });
  });

  it('logs both sides of the conversation without audio payloads', () => {
    const logger = new RecordingLogger();
    const { twilio, elevenlabs } = startElevenLabsBridge({ logger });
    openElevenLabsStream(twilio, elevenlabs);

    elevenlabs.emit({ type: 'user_transcript', user_transcription_event: { user_transcript: 'שלום' } });
    elevenlabs.emit({ type: 'agent_response', agent_response_event: { agent_response: 'היי' } });

    expect(logger.messages('[conversation]')).toEqual(['[conversation] USER: שלום', '[conversation] AI: היי']);
    expect(JSON.stringify(logger.lines)).not.toContain('audio_base_64');
  });

  it('hangs up through Twilio once the goodbye has drained', async () => {
    const ended: string[] = [];
    const { twilio, elevenlabs } = startElevenLabsBridge({
      endCall: async (reason) => {
        ended.push(reason);
      },
    });
    openElevenLabsStream(twilio, elevenlabs);

    elevenlabs.emit({ type: 'audio', audio_event: { audio_base_64: 'Ynll', event_id: 1 } });
    elevenlabs.emit({
      type: 'client_tool_call',
      client_tool_call: { tool_name: 'end_call', tool_call_id: 'tc_1', parameters: { reason: 'caller_said_goodbye' } },
    });

    // The tool call is answered, but the call survives until Twilio plays the goodbye.
    expect(elevenlabs.sent.at(-1)).toEqual(
      expect.objectContaining({ type: 'client_tool_result', tool_call_id: 'tc_1', is_error: false }),
    );
    expect(ended).toEqual([]);

    twilio.emit({ event: 'mark', mark: { name: 'callora-assistant-audio' } });
    await flush();
    expect(ended).toEqual(['caller_said_goodbye']);
  });

  it('hangs up once even if the agent calls end_call twice', async () => {
    const ended: string[] = [];
    const { twilio, elevenlabs } = startElevenLabsBridge({
      endCall: async (reason) => {
        ended.push(reason);
      },
    });
    openElevenLabsStream(twilio, elevenlabs);

    for (const toolCallId of ['tc_1', 'tc_2']) {
      elevenlabs.emit({
        type: 'client_tool_call',
        client_tool_call: { tool_name: 'end_call', tool_call_id: toolCallId, parameters: { reason: 'request_completed' } },
      });
    }
    await flush();

    expect(ended).toEqual(['request_completed']);
  });

  it('reports an unknown client tool as an error instead of hanging up', () => {
    const { twilio, elevenlabs } = startElevenLabsBridge();
    openElevenLabsStream(twilio, elevenlabs);

    elevenlabs.emit({
      type: 'client_tool_call',
      client_tool_call: { tool_name: 'transfer_funds', tool_call_id: 'tc_9', parameters: {} },
    });

    expect(elevenlabs.sent.at(-1)).toEqual(
      expect.objectContaining({ type: 'client_tool_result', tool_call_id: 'tc_9', is_error: true }),
    );
  });

  it('refuses a stream whose CallSid does not match the authorized call', () => {
    const { twilio, elevenlabs } = startElevenLabsBridge();
    twilio.emit({ event: 'start', streamSid, start: { streamSid, callSid: 'CAOTHER' } });

    expect(twilio.closed).toBe(true);
    expect(elevenlabs.closed).toBe(true);
  });

  // Either format being wrong is enough: Twilio only speaks mu-law, and this bridge
  // never transcodes, so a mismatched session can only produce noise.
  const mismatchedFormats = [
    { agent_output_audio_format: 'pcm_16000', user_input_audio_format: ELEVENLABS_AUDIO_FORMAT },
    { agent_output_audio_format: ELEVENLABS_AUDIO_FORMAT, user_input_audio_format: 'pcm_16000' },
    { agent_output_audio_format: 'pcm_16000', user_input_audio_format: 'pcm_16000' },
  ];

  it.each(mismatchedFormats)('closes the call rather than transcoding %o', (formats) => {
    const logger = new RecordingLogger();
    const { twilio, elevenlabs } = startElevenLabsBridge({ logger });
    twilio.emit({ event: 'start', streamSid, start: { streamSid, callSid } });
    elevenlabs.emit({
      type: 'conversation_initiation_metadata',
      conversation_initiation_metadata_event: { conversation_id: 'conv_123', ...formats },
    });

    expect(twilio.closed).toBe(true);
    expect(elevenlabs.closed).toBe(true);

    const error = logger.lines.find((line) => line.level === 'error');
    expect(error?.message).toMatch(/mu-law 8 kHz/i);
    // The log names what was actually negotiated, so the misconfiguration is obvious.
    expect(error?.details).toEqual(expect.objectContaining({ expected: ELEVENLABS_AUDIO_FORMAT }));

    // Nothing reached the caller, before or after the mismatch was detected.
    const before = twilio.sent.length;
    elevenlabs.emit({ type: 'audio', audio_event: { audio_base_64: 'YQ==', event_id: 1 } });
    expect(twilio.sent).toHaveLength(before);
    expect(twilio.sent.filter((message) => message['event'] === 'media')).toHaveLength(0);
  });

  it('closes both sides exactly once when either end disconnects', () => {
    const { twilio, elevenlabs } = startElevenLabsBridge();
    openElevenLabsStream(twilio, elevenlabs);

    elevenlabs.emitClose();
    expect(twilio.closed).toBe(true);
    expect(elevenlabs.closed).toBe(true);
  });
});

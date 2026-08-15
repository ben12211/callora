import { describe, expect, it } from 'vitest';
import { loadConfig } from '../src/config.js';
import { CartesiaBridge } from '../src/realtime/cartesia-bridge.js';
import {
  CARTESIA_AUDIO_ENCODING,
  CARTESIA_SAMPLE_RATE,
  DEFAULT_CARTESIA_STT_MODEL,
  DEFAULT_CARTESIA_TTS_MODEL,
} from '../src/realtime/cartesia-constants.js';
import type { CartesiaSocket } from '../src/realtime/cartesia-connection.js';
import {
  CARTESIA_OUTPUT_FORMAT,
  buildSttUrl,
  buildTtsCancel,
  buildTtsChunk,
  buildTtsUrl,
  cartesiaLanguage,
} from '../src/realtime/cartesia-protocol.js';
import { REALTIME_PROVIDERS } from '../src/realtime/provider.js';
import { streamChatCompletion, type StreamChatOptions, type StreamChatResult } from '../src/realtime/text-llm.js';
import { FakeChannel, RecordingLogger, agent, businessId, callSid, flush, streamSid } from './support/realtime-harness.js';

const baseEnv = {
  NODE_ENV: 'test',
  DATABASE_URL: 'postgresql://unused',
  TWILIO_ACCOUNT_SID: 'AC00000000000000000000000000000000',
  TWILIO_AUTH_TOKEN: 'test-auth-token',
  PUBLIC_BASE_URL: 'https://voice.example.test',
} satisfies NodeJS.ProcessEnv;

const cartesiaEnv = {
  ...baseEnv,
  VOICE_PROVIDER: 'cartesia',
  CARTESIA_API_KEY: 'sk_car_test',
  CARTESIA_VOICE_ID: 'voice-uuid',
  OPENAI_API_KEY: 'sk-llm',
};

describe('cartesia provider selection', () => {
  it('is offered alongside the existing providers', () => {
    expect([...REALTIME_PROVIDERS]).toEqual(['openai', 'elevenlabs', 'cartesia']);
  });

  it('selects cartesia with its own credentials and sane defaults', () => {
    const config = loadConfig(cartesiaEnv);
    expect(config.voiceProvider).toBe('cartesia');
    if (config.voiceProvider !== 'cartesia') throw new Error('expected cartesia');

    expect(config.cartesiaApiKey).toBe('sk_car_test');
    expect(config.cartesiaVoiceId).toBe('voice-uuid');
    expect(config.cartesiaTtsModel).toBe(DEFAULT_CARTESIA_TTS_MODEL);
    expect(config.cartesiaSttModel).toBe(DEFAULT_CARTESIA_STT_MODEL);
    // Cartesia covers speech only, so the reasoning turn reuses the OpenAI key.
    expect(config.textLlmApiKey).toBe('sk-llm');
  });

  it('requires the cartesia credentials and the LLM key, naming each', () => {
    expect(() => loadConfig({ ...cartesiaEnv, CARTESIA_API_KEY: '' })).toThrow(/CARTESIA_API_KEY/);
    expect(() => loadConfig({ ...cartesiaEnv, CARTESIA_VOICE_ID: '' })).toThrow(/CARTESIA_VOICE_ID/);
    expect(() => loadConfig({ ...cartesiaEnv, OPENAI_API_KEY: '' })).toThrow(/OPENAI_API_KEY/);
  });

  it('does not require cartesia credentials for the other providers', () => {
    expect(() => loadConfig({ ...baseEnv, VOICE_PROVIDER: 'openai', OPENAI_API_KEY: 'sk' })).not.toThrow();
    expect(() =>
      loadConfig({
        ...baseEnv,
        VOICE_PROVIDER: 'elevenlabs',
        ELEVENLABS_API_KEY: 'xi',
        ELEVENLABS_AGENT_ID: 'a',
        CARTESIA_API_KEY: '',
        CARTESIA_VOICE_ID: '',
      }),
    ).not.toThrow();
  });
});

describe('cartesia wire format', () => {
  it('asks both sockets for mu-law 8k so nothing is transcoded', () => {
    const sttUrl = new URL(buildSttUrl({ baseUrl: 'wss://api.cartesia.ai', model: 'ink-whisper', version: '2026-03-01', language: 'he' }));

    expect(sttUrl.pathname).toBe('/stt/websocket');
    expect(sttUrl.searchParams.get('encoding')).toBe('pcm_mulaw');
    expect(sttUrl.searchParams.get('sample_rate')).toBe('8000');
    expect(sttUrl.searchParams.get('language')).toBe('he');
    expect(sttUrl.searchParams.get('cartesia_version')).toBe('2026-03-01');

    expect(CARTESIA_OUTPUT_FORMAT).toEqual({ container: 'raw', encoding: 'pcm_mulaw', sample_rate: 8000 });
    expect(CARTESIA_AUDIO_ENCODING).toBe('pcm_mulaw');
    expect(CARTESIA_SAMPLE_RATE).toBe(8000);
  });

  it('never puts the API key in a socket URL', () => {
    const stt = buildSttUrl({ baseUrl: 'wss://api.cartesia.ai', model: 'ink-whisper', version: '2026-03-01' });
    const tts = buildTtsUrl({ baseUrl: 'wss://api.cartesia.ai', version: '2026-03-01' });
    for (const url of [stt, tts]) {
      expect(url).not.toMatch(/api_key|access_token|sk_car/);
    }
  });

  it('normalizes Hebrew to the bare code Cartesia expects', () => {
    for (const locale of ['he-IL', 'he', 'he_IL', 'HE-il']) {
      expect(cartesiaLanguage({ ...agent, language: locale })).toBe('he');
    }
    expect(cartesiaLanguage({ ...agent, language: 'en-US' })).toBe('en');
  });

  it('builds a generation request in the documented shape', () => {
    const chunk = buildTtsChunk({
      model: DEFAULT_CARTESIA_TTS_MODEL,
      voiceId: 'voice-uuid',
      contextId: 'ctx-1',
      transcript: 'שלום',
      language: 'he',
      continue: true,
    });

    expect(chunk).toEqual({
      model_id: DEFAULT_CARTESIA_TTS_MODEL,
      transcript: 'שלום',
      voice: { mode: 'id', id: 'voice-uuid' },
      language: 'he',
      context_id: 'ctx-1',
      output_format: { container: 'raw', encoding: 'pcm_mulaw', sample_rate: 8000 },
      continue: true,
    });
  });

  it('cancels a context by id with no type field', () => {
    expect(buildTtsCancel('ctx-1')).toEqual({ context_id: 'ctx-1', cancel: true });
  });
});

/** In-memory stand-in for a Cartesia socket, recording text and binary separately. */
class FakeCartesiaSocket implements CartesiaSocket {
  public readonly text: Record<string, unknown>[] = [];
  public readonly rawText: string[] = [];
  public readonly binary: Buffer[] = [];
  public closed = false;
  private messageHandler: ((raw: string) => void) | null = null;
  private closeHandler: (() => void) | null = null;

  public sendText(payload: string): void {
    this.rawText.push(payload);
    try {
      this.text.push(JSON.parse(payload) as Record<string, unknown>);
    } catch {
      // Bare commands like `finalize` are not JSON; rawText keeps them.
    }
  }
  public sendBinary(payload: Buffer): void {
    this.binary.push(payload);
  }
  public close(): void {
    this.closed = true;
  }
  public onMessage(handler: (raw: string) => void): void {
    this.messageHandler = handler;
  }
  public onClose(handler: () => void): void {
    this.closeHandler = handler;
  }
  public onError(): void {}
  public emit(message: Record<string, unknown>): void {
    this.messageHandler?.(JSON.stringify(message));
  }
  public emitClose(): void {
    this.closeHandler?.();
  }
}

interface StartedCartesia {
  twilio: FakeChannel;
  stt: FakeCartesiaSocket;
  tts: FakeCartesiaSocket;
  bridge: CartesiaBridge;
  logger: RecordingLogger;
}

function startCartesia(
  overrides: Partial<ConstructorParameters<typeof CartesiaBridge>[0]> = {},
): StartedCartesia {
  const twilio = new FakeChannel();
  const stt = new FakeCartesiaSocket();
  const tts = new FakeCartesiaSocket();
  const logger = (overrides.logger as RecordingLogger) ?? new RecordingLogger();
  const bridge = new CartesiaBridge({
    twilio,
    stt,
    tts,
    agent,
    businessId,
    callSid,
    ttsModel: DEFAULT_CARTESIA_TTS_MODEL,
    voiceId: 'voice-uuid',
    llm: { baseUrl: 'https://llm.invalid/v1', apiKey: 'sk-llm', model: 'gpt-4o-mini' },
    logger,
    streamChat: async () => ({ text: '', toolCalls: [], aborted: false }),
    ...overrides,
  });
  bridge.start();
  return { twilio, stt, tts, bridge, logger };
}

function openStream(twilio: FakeChannel): void {
  twilio.emit({ event: 'start', streamSid, start: { streamSid, callSid } });
}

/** The context id the bridge used for its most recent utterance. */
function lastContext(tts: FakeCartesiaSocket): string {
  const withContext = tts.text.filter((message) => typeof message['context_id'] === 'string');
  return String(withContext[withContext.length - 1]?.['context_id']);
}

describe('cartesia bridge', () => {
  it('speaks the tenant greeting in Hebrew when the stream opens', () => {
    const { twilio, tts } = startCartesia();
    openStream(twilio);

    const greeting = tts.text.find((message) => message['transcript'] === agent.greeting);
    expect(greeting).toBeDefined();
    expect(greeting?.['language']).toBe('he');
    expect(greeting?.['output_format']).toEqual({ container: 'raw', encoding: 'pcm_mulaw', sample_rate: 8000 });
  });

  it('forwards caller audio as raw bytes, not base64', () => {
    const { twilio, stt } = startCartesia();
    openStream(twilio);

    const payload = Buffer.from([0xff, 0x7f, 0x00]).toString('base64');
    twilio.emit({ event: 'media', media: { timestamp: '20', payload } });

    expect(stt.binary.at(-1)).toEqual(Buffer.from([0xff, 0x7f, 0x00]));
    // The samples are unchanged: base64 decoding is not a transcode.
    expect(stt.binary.at(-1)?.length).toBe(3);
  });

  it('returns Sonic audio to Twilio with a mark for each chunk', () => {
    const { twilio, tts } = startCartesia();
    openStream(twilio);
    const context = lastContext(tts);

    tts.emit({ type: 'chunk', data: 'YWdlbnQ=', context_id: context });

    expect(twilio.sent).toEqual([
      { event: 'media', streamSid, media: { payload: 'YWdlbnQ=' } },
      { event: 'mark', streamSid, mark: { name: 'callora-assistant-audio' } },
    ]);
  });

  it('runs a turn and streams the reply into Sonic before it is finished', async () => {
    const seen: string[] = [];
    const streamChat = async (options: StreamChatOptions): Promise<StreamChatResult> => {
      // Two clauses, delivered as separate deltas.
      options.onTextDelta?.('בוודאי, ');
      options.onTextDelta?.('אני בודק עכשיו. ');
      return { text: 'בוודאי, אני בודק עכשיו. ', toolCalls: [], aborted: false };
    };
    const { twilio, stt, tts } = startCartesia({
      streamChat: streamChat as typeof streamChatCompletion,
    });
    openStream(twilio);
    tts.text.length = 0;

    stt.emit({ type: 'transcript', is_final: true, text: 'מה המצב?' });
    await flush();

    const turn = tts.text.filter((message) => typeof message['transcript'] === 'string');
    seen.push(...turn.map((message) => String(message['transcript'])));
    // Speech started before the reply was complete: at least one continuing fragment.
    expect(turn.some((message) => message['continue'] === true)).toBe(true);
    // ...and the context was closed at the end.
    expect(turn.at(-1)?.['continue']).toBe(false);
    expect(seen.join('')).toContain('בוודאי');
  });

  it('logs both sides of the conversation without audio payloads', async () => {
    const { twilio, stt, tts, logger } = startCartesia({
      streamChat: (async (options: StreamChatOptions) => {
        options.onTextDelta?.('כן. ');
        return { text: 'כן. ', toolCalls: [], aborted: false };
      }) as typeof streamChatCompletion,
    });
    openStream(twilio);
    stt.emit({ type: 'transcript', is_final: true, text: 'שלום' });
    await flush();
    tts.emit({ type: 'chunk', data: 'c2hvdWxkLW5vdC1sb2c=', context_id: lastContext(tts) });

    const conversation = logger.messages('[conversation]');
    expect(conversation).toContain('[conversation] USER: שלום');
    expect(conversation.some((line) => line.startsWith('[conversation] AI:'))).toBe(true);
    expect(JSON.stringify(logger.lines)).not.toContain('c2hvdWxkLW5vdC1sb2c=');
  });

  it('ignores partial transcripts for the turn text and only acts on finals', async () => {
    let calls = 0;
    const { twilio, stt } = startCartesia({
      streamChat: (async () => {
        calls += 1;
        return { text: '', toolCalls: [], aborted: false };
      }) as typeof streamChatCompletion,
    });
    openStream(twilio);

    stt.emit({ type: 'transcript', is_final: false, text: 'מה' });
    await flush();
    expect(calls).toBe(0);

    stt.emit({ type: 'transcript', is_final: true, text: 'מה שלומך' });
    await flush();
    expect(calls).toBe(1);
  });

  it('concatenates final transcript deltas verbatim', async () => {
    let received = '';
    const { twilio, stt } = startCartesia({
      streamChat: (async (options: StreamChatOptions) => {
        received = String(options.messages.at(-1)?.content ?? '');
        return { text: '', toolCalls: [], aborted: false };
      }) as typeof streamChatCompletion,
    });
    openStream(twilio);

    // Cartesia sends finals as deltas that must be joined without adding whitespace.
    stt.emit({ type: 'transcript', is_final: true, text: 'שלום ' });
    await flush();
    expect(received).toBe('שלום');
  });

  describe('barge-in', () => {
    it('cancels the context, clears Twilio, and starts listening again', () => {
      const { twilio, stt, tts } = startCartesia();
      openStream(twilio);
      const context = lastContext(tts);
      tts.emit({ type: 'chunk', data: 'YQ==', context_id: context });
      twilio.sent.length = 0;
      tts.text.length = 0;

      stt.emit({ type: 'transcript', is_final: false, text: 'רגע' });

      expect(tts.text.at(-1)).toEqual({ context_id: context, cancel: true });
      expect(twilio.sent.at(-1)).toEqual({ event: 'clear', streamSid });
    });

    // Cartesia's cancel only stops generations that have not started; an in-flight one
    // keeps streaming. Dropping those chunks is what actually silences the agent.
    it('discards audio that arrives after the context was abandoned', () => {
      const { twilio, stt, tts } = startCartesia();
      openStream(twilio);
      const context = lastContext(tts);
      tts.emit({ type: 'chunk', data: 'YQ==', context_id: context });

      stt.emit({ type: 'transcript', is_final: false, text: 'רגע' });
      twilio.sent.length = 0;

      // Late audio from the cancelled generation.
      tts.emit({ type: 'chunk', data: 'bGF0ZQ==', context_id: context });
      expect(twilio.sent.filter((message) => message['event'] === 'media')).toHaveLength(0);
    });

    it('does not fire on an empty partial', () => {
      const { twilio, stt, tts } = startCartesia();
      openStream(twilio);
      tts.emit({ type: 'chunk', data: 'YQ==', context_id: lastContext(tts) });
      twilio.sent.length = 0;
      tts.text.length = 0;

      stt.emit({ type: 'transcript', is_final: false, text: '   ' });
      expect(tts.text).toHaveLength(0);
      expect(twilio.sent).toHaveLength(0);
    });
  });

  it('hangs up through Twilio once the goodbye has drained', async () => {
    const ended: string[] = [];
    const { twilio, stt, tts } = startCartesia({
      endCall: async (reason) => {
        ended.push(reason);
      },
      streamChat: (async () => ({
        text: 'להתראות!',
        toolCalls: [
          { id: 'c1', type: 'function' as const, function: { name: 'end_call', arguments: '{"reason":"caller_said_goodbye"}' } },
        ],
        aborted: false,
      })) as typeof streamChatCompletion,
    });
    openStream(twilio);
    stt.emit({ type: 'transcript', is_final: true, text: 'ביי' });
    await flush();

    const context = lastContext(tts);
    // The LLM reports end_call before Sonic has produced any audio. Hanging up here
    // would cut the caller off mid-goodbye, so the bridge must still be waiting.
    expect(ended).toEqual([]);

    tts.emit({ type: 'chunk', data: 'Ynll', context_id: context });
    expect(ended).toEqual([]);

    // Generation finished, but Twilio has not confirmed playback yet.
    tts.emit({ type: 'done', context_id: context });
    await flush();
    expect(ended).toEqual([]);

    twilio.emit({ event: 'mark', mark: { name: 'callora-assistant-audio' } });
    await flush();
    expect(ended).toEqual(['caller_said_goodbye']);
  });

  it('still hangs up if the caller talks over the goodbye', async () => {
    const ended: string[] = [];
    const { twilio, stt, tts } = startCartesia({
      endCall: async (reason) => {
        ended.push(reason);
      },
      streamChat: (async () => ({
        text: 'להתראות!',
        toolCalls: [
          { id: 'c1', type: 'function' as const, function: { name: 'end_call', arguments: '{"reason":"request_completed"}' } },
        ],
        aborted: false,
      })) as typeof streamChatCompletion,
    });
    openStream(twilio);
    stt.emit({ type: 'transcript', is_final: true, text: 'תודה' });
    await flush();
    tts.emit({ type: 'chunk', data: 'Ynll', context_id: lastContext(tts) });

    // Barging in over the farewell must not strand the call waiting for a `done`
    // that the cancelled context will never send.
    stt.emit({ type: 'transcript', is_final: false, text: 'רגע' });
    await flush();
    expect(ended).toEqual(['interrupted-farewell']);
  });

  it('refuses a stream whose CallSid does not match the authorized call', () => {
    const { twilio, stt, tts } = startCartesia();
    twilio.emit({ event: 'start', streamSid, start: { streamSid, callSid: 'CAOTHER' } });

    expect(twilio.closed).toBe(true);
    expect(stt.closed).toBe(true);
    expect(tts.closed).toBe(true);
  });

  it('tears down all three streams when any one disconnects', () => {
    const { twilio, stt, tts } = startCartesia();
    openStream(twilio);

    stt.emitClose();
    expect(twilio.closed).toBe(true);
    expect(tts.closed).toBe(true);
  });

  it('surfaces a Cartesia TTS error without leaking the key', () => {
    const { twilio, tts, logger } = startCartesia();
    openStream(twilio);

    tts.emit({ type: 'error', error_code: 'bad_voice', message: 'voice not found', status_code: 400 });

    const error = logger.lines.find((line) => line.level === 'error');
    expect(error?.details).toEqual(expect.objectContaining({ code: 'bad_voice', reason: 'voice not found' }));
    expect(JSON.stringify(logger.lines)).not.toContain('sk_car');
  });
});

describe('streaming text llm', () => {
  function sseResponse(frames: readonly string[]): Response {
    const body = new ReadableStream<Uint8Array>({
      start(controller) {
        const encoder = new TextEncoder();
        for (const frame of frames) {
          controller.enqueue(encoder.encode(frame));
        }
        controller.close();
      },
    });
    return new Response(body, { status: 200 });
  }

  const options = {
    baseUrl: 'https://llm.invalid/v1',
    apiKey: 'sk-secret',
    model: 'gpt-4o-mini',
    messages: [{ role: 'user' as const, content: 'hi' }],
  };

  it('surfaces deltas in order as they arrive', async () => {
    const deltas: string[] = [];
    const fetchImpl = (async () =>
      sseResponse([
        'data: {"choices":[{"delta":{"content":"Hel"}}]}\n\n',
        'data: {"choices":[{"delta":{"content":"lo"}}]}\n\n',
        'data: [DONE]\n\n',
      ])) as unknown as typeof fetch;

    const result = await streamChatCompletion({ ...options, fetchImpl, onTextDelta: (d) => deltas.push(d) });
    expect(deltas).toEqual(['Hel', 'lo']);
    expect(result.text).toBe('Hello');
  });

  it('reassembles a tool call split across chunks', async () => {
    const fetchImpl = (async () =>
      sseResponse([
        'data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"end_","arguments":"{\\"reason\\""}}]}}]}\n\n',
        'data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"call","arguments":":\\"done\\"}"}}]}}]}\n\n',
        'data: [DONE]\n\n',
      ])) as unknown as typeof fetch;

    const result = await streamChatCompletion({ ...options, fetchImpl });
    expect(result.toolCalls).toEqual([
      { id: 'c1', type: 'function', function: { name: 'end_call', arguments: '{"reason":"done"}' } },
    ]);
  });

  it('handles a frame split across two reads', async () => {
    const fetchImpl = (async () =>
      sseResponse(['data: {"choices":[{"delta":{"con', 'tent":"split"}}]}\n\n', 'data: [DONE]\n\n'])) as unknown as typeof fetch;

    const result = await streamChatCompletion({ ...options, fetchImpl });
    expect(result.text).toBe('split');
  });

  it('reports a failure by status without echoing the key', async () => {
    const fetchImpl = (async () => new Response('nope', { status: 401 })) as unknown as typeof fetch;
    await expect(streamChatCompletion({ ...options, fetchImpl })).rejects.toThrow(/status 401/);
    await expect(streamChatCompletion({ ...options, fetchImpl })).rejects.not.toThrow(/sk-secret/);
  });

  it('resolves as aborted rather than throwing when barged in', async () => {
    const controller = new AbortController();
    controller.abort();
    const fetchImpl = (async () => {
      throw new Error('aborted');
    }) as unknown as typeof fetch;

    const result = await streamChatCompletion({ ...options, fetchImpl, signal: controller.signal });
    expect(result.aborted).toBe(true);
  });
});

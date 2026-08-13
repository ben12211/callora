import { describe, expect, it } from 'vitest';
import { createStreamToken, verifyStreamToken } from '../src/http/stream-token.js';
import { MediaStreamBridge, type BridgeLogger, type MessageChannel } from '../src/realtime/bridge.js';
import { buildSessionUpdate } from '../src/realtime/protocol.js';
import type { AgentConfig } from '../src/domain/models.js';

const businessId = '00000000-0000-4000-8000-000000000001';
const callSid = 'CABRIDGE1';
const streamSid = 'MZ0000000000000000000000000000';

const agent: AgentConfig = {
  businessId,
  instructions: 'Be concise.',
  greeting: 'שלום, איך אפשר לעזור?',
  language: 'he-IL',
  voice: 'marin',
  realtimeModel: 'gpt-realtime-2.1',
  enabled: true,
  createdAt: new Date(),
  updatedAt: new Date(),
};

const silentLogger: BridgeLogger = { info: () => {}, warn: () => {}, error: () => {} };

class FakeChannel implements MessageChannel {
  public readonly sent: Record<string, unknown>[] = [];
  public closed = false;
  private messageHandler: ((raw: string) => void) | null = null;
  private closeHandler: (() => void) | null = null;

  public send(payload: string): void {
    this.sent.push(JSON.parse(payload) as Record<string, unknown>);
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

  public types(): string[] {
    return this.sent.map((message) => String(message['type'] ?? message['event']));
  }
}

function startBridge(): { twilio: FakeChannel; openai: FakeChannel; bridge: MediaStreamBridge } {
  const twilio = new FakeChannel();
  const openai = new FakeChannel();
  const bridge = new MediaStreamBridge({
    twilio,
    openai,
    agent,
    businessId,
    callSid,
    logger: silentLogger,
  });
  bridge.start();
  return { twilio, openai, bridge };
}

function openStream(twilio: FakeChannel, openai: FakeChannel): void {
  twilio.emit({ event: 'start', streamSid, start: { streamSid, callSid, accountSid: 'AC1' } });
  openai.emit({ type: 'session.created', session: { id: 'sess_123' } });
}

describe('media stream security token', () => {
  it('round-trips call-scoped claims', () => {
    const token = createStreamToken('secret', { callSid, businessId });
    expect(verifyStreamToken('secret', token)).toEqual(
      expect.objectContaining({ callSid, businessId }),
    );
  });

  it('rejects a wrong secret, a tampered payload, and an expired token', () => {
    const token = createStreamToken('secret', { callSid, businessId });
    expect(verifyStreamToken('other-secret', token)).toBeNull();
    expect(verifyStreamToken('secret', `x${token}`)).toBeNull();

    const expired = createStreamToken('secret', { callSid, businessId }, 60, 0);
    expect(verifyStreamToken('secret', expired)).toBeNull();
  });
});

describe('realtime session configuration', () => {
  it('bridges telephony audio directly as pcmu in both directions', () => {
    const update = buildSessionUpdate({ agent }) as {
      session: {
        type: string;
        model: string;
        audio: {
          input: { format: { type: string }; turn_detection: { type: string } };
          output: { format: { type: string }; voice: string };
        };
      };
    };

    expect(update.session.type).toBe('realtime');
    expect(update.session.model).toBe('gpt-realtime-2.1');
    expect(update.session.audio.input.format.type).toBe('audio/pcmu');
    expect(update.session.audio.output.format.type).toBe('audio/pcmu');
    expect(update.session.audio.output.voice).toBe('marin');
    expect(update.session.audio.input.turn_detection.type).toBe('server_vad');
  });
});

describe('media stream bridge', () => {
  it('configures the session and greets the caller once the session is ready', () => {
    const { twilio, openai } = startBridge();
    expect(openai.types()).toEqual(['session.update']);

    openStream(twilio, openai);
    expect(openai.types()).toContain('response.create');
    expect(JSON.stringify(openai.sent)).toContain(agent.greeting);

    // A later session.updated must not trigger a second greeting.
    openai.emit({ type: 'session.updated', session: { id: 'sess_123' } });
    expect(openai.types().filter((type) => type === 'response.create')).toHaveLength(1);
  });

  it('forwards caller audio to OpenAI and assistant audio back to Twilio', () => {
    const { twilio, openai } = startBridge();
    openStream(twilio, openai);

    twilio.emit({ event: 'media', media: { timestamp: '20', payload: 'Y2FsbGVy' } });
    expect(openai.sent.at(-1)).toEqual({ type: 'input_audio_buffer.append', audio: 'Y2FsbGVy' });

    openai.emit({ type: 'response.output_audio.delta', delta: 'YXNzaXN0YW50', item_id: 'item_1' });
    expect(twilio.sent).toEqual([
      { event: 'media', streamSid, media: { payload: 'YXNzaXN0YW50' } },
      { event: 'mark', streamSid, mark: { name: 'callora-assistant-audio' } },
    ]);
  });

  it('clears queued audio and truncates the item when the caller barges in', () => {
    const { twilio, openai } = startBridge();
    openStream(twilio, openai);

    twilio.emit({ event: 'media', media: { timestamp: '1000', payload: 'YQ==' } });
    openai.emit({ type: 'response.output_audio.delta', delta: 'Yg==', item_id: 'item_1' });
    twilio.emit({ event: 'media', media: { timestamp: '1600', payload: 'Yw==' } });
    openai.emit({ type: 'input_audio_buffer.speech_started' });

    expect(openai.sent.at(-1)).toEqual({
      type: 'conversation.item.truncate',
      item_id: 'item_1',
      content_index: 0,
      audio_end_ms: 600,
    });
    expect(twilio.sent.at(-1)).toEqual({ event: 'clear', streamSid });
  });

  it('ignores barge-in when no assistant audio is queued', () => {
    const { twilio, openai } = startBridge();
    openStream(twilio, openai);
    const before = openai.sent.length;

    openai.emit({ type: 'input_audio_buffer.speech_started' });
    expect(openai.sent).toHaveLength(before);
    expect(twilio.sent).toHaveLength(0);
  });

  it('refuses a stream whose CallSid does not match the authorized call', () => {
    const { twilio, openai } = startBridge();
    twilio.emit({ event: 'start', streamSid, start: { streamSid, callSid: 'CAOTHER' } });

    expect(twilio.closed).toBe(true);
    expect(openai.closed).toBe(true);
  });

  it('closes both sides exactly once when either end disconnects', () => {
    const { twilio, openai } = startBridge();
    openStream(twilio, openai);

    twilio.emit({ event: 'stop' });
    expect(twilio.closed).toBe(true);
    expect(openai.closed).toBe(true);

    // A subsequent close from the other side must be a no-op.
    openai.emitClose();
    expect(openai.closed).toBe(true);
  });

  it('reports the stream and OpenAI session identifiers for persistence', () => {
    const twilio = new FakeChannel();
    const openai = new FakeChannel();
    const seen: { streamSid: string | null; openaiSessionId: string | null }[] = [];
    new MediaStreamBridge({
      twilio,
      openai,
      agent,
      businessId,
      callSid,
      logger: silentLogger,
      onIdentifiers: (identifiers) => seen.push(identifiers),
    }).start();

    openStream(twilio, openai);
    expect(seen.at(-1)).toEqual({ streamSid, openaiSessionId: 'sess_123' });
  });
});

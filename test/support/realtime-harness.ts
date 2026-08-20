import {
  MediaStreamBridge,
  type BridgeLogger,
  type BridgeOptions,
  type MessageChannel,
} from '../../src/realtime/bridge.js';
import { END_CALL_TOOL_NAME } from '../../src/realtime/protocol.js';
import type { AgentConfig } from '../../src/domain/models.js';

export const businessId = '00000000-0000-4000-8000-000000000001';
export const callSid = 'CABRIDGE1';
export const streamSid = 'MZ0000000000000000000000000000';

export const agent: AgentConfig = {
  businessId,
  instructions: 'Be concise.',
  greeting: 'שלום, איך אפשר לעזור?',
  language: 'he-IL',
  voice: 'marin',
  realtimeModel: 'gpt-realtime-2.1',
  voiceProvider: 'openai',
  elevenLabsAgentId: '',
  enabled: true,
  createdAt: new Date(),
  updatedAt: new Date(),
};

export const silentLogger: BridgeLogger = {
  debug: () => {},
  info: () => {},
  warn: () => {},
  error: () => {},
};

export interface LogLine {
  level: 'debug' | 'info' | 'warn' | 'error';
  details: Record<string, unknown>;
  message: string;
}

/** Captures what the bridge logged, so log content can be asserted directly. */
export class RecordingLogger implements BridgeLogger {
  public readonly lines: LogLine[] = [];

  public debug(details: Record<string, unknown>, message: string): void {
    this.lines.push({ level: 'debug', details, message });
  }

  public info(details: Record<string, unknown>, message: string): void {
    this.lines.push({ level: 'info', details, message });
  }

  public warn(details: Record<string, unknown>, message: string): void {
    this.lines.push({ level: 'warn', details, message });
  }

  public error(details: Record<string, unknown>, message: string): void {
    this.lines.push({ level: 'error', details, message });
  }

  public messages(prefix = ''): string[] {
    return this.lines.map((line) => line.message).filter((message) => message.startsWith(prefix));
  }
}

/** In-memory stand-in for either end of the bridge. */
export class FakeChannel implements MessageChannel {
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

export interface StartedBridge {
  twilio: FakeChannel;
  openai: FakeChannel;
  bridge: MediaStreamBridge;
}

export function startBridge(overrides: Partial<BridgeOptions> = {}): StartedBridge {
  const twilio = new FakeChannel();
  const openai = new FakeChannel();
  const bridge = new MediaStreamBridge({
    twilio,
    openai,
    agent,
    businessId,
    callSid,
    logger: silentLogger,
    ...overrides,
  });
  bridge.start();
  return { twilio, openai, bridge };
}

export function openStream(twilio: FakeChannel, openai: FakeChannel): void {
  twilio.emit({ event: 'start', streamSid, start: { streamSid, callSid, accountSid: 'AC1' } });
  openai.emit({ type: 'session.created', session: { id: 'sess_123' } });
}

/** Lets the bridge's `endCall` promise settle before assertions. */
export async function flush(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
}

/** One assistant audio chunk, acknowledged by Twilio as actually played. */
export function speakAndPlay(twilio: FakeChannel, openai: FakeChannel, itemId = 'item_bye'): void {
  openai.emit({ type: 'response.output_audio.delta', delta: 'Ynll', item_id: itemId });
  twilio.emit({ event: 'mark', mark: { name: 'callora-assistant-audio' } });
}

export function emitEndCall(openai: FakeChannel, callId = 'call_1', reason = 'caller_said_goodbye'): void {
  openai.emit({
    type: 'response.function_call_arguments.done',
    name: END_CALL_TOOL_NAME,
    call_id: callId,
    arguments: JSON.stringify({ reason }),
  });
}

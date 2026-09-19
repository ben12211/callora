import WebSocket from 'ws';
import type { SpeechPreparation } from '../hebrew/types.js';
import type { SpeechSynthesisSession } from './tts-session.js';
import { parseJsonObject, readString } from './protocol.js';

export interface DeepdubConnectOptions {
  apiKey: string;
  url: string;
  model: string;
  voiceId: string;
  locale: string;
  firstAudioTimeoutMs: number;
  enableLogging: boolean;
  targetGender?: 'male' | 'female';
  connectTimeoutMs?: number;
}

const IPA = /^(?:[\p{L}\p{M}\s]|[ˈˌː‿-])+$/u;

function escapeXml(value: string): string {
  return value.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;').replace(/'/g, '&apos;');
}

function replaceAllLiteral(value: string, source: string, replacement: string): string {
  return value.split(source).join(replacement);
}

/** Deepdub owns SSML rendering; generic preprocessing never emits provider markup. */
export function renderDeepdubText(preparation: SpeechPreparation): string {
  let rendered = escapeXml(preparation.spokenText);
  for (const span of [...preparation.spans].sort((a, b) => b.sourceText.length - a.sourceText.length)) {
    const source = escapeXml(span.sourceText);
    if (!source || !rendered.includes(source)) continue;
    if (span.type === 'replacement') {
      rendered = replaceAllLiteral(rendered, source, escapeXml(span.pronunciation));
      continue;
    }
    if (!IPA.test(span.pronunciation)) continue;
    rendered = replaceAllLiteral(rendered, source, `<phoneme alphabet="ipa" ph="${escapeXml(span.pronunciation)}">${source}</phoneme>`);
  }
  return rendered;
}

export class DeepdubTtsSession implements SpeechSynthesisSession {
  private activeContextId: string | null = null;
  private connectionId: string | null = null;
  private cancelPending = false;
  private readonly deferred: Array<() => void> = [];
  private audioHandler: (event: { contextId: string; data: string }) => void = () => {};
  private doneHandler: (contextId: string) => void = () => {};
  private errorHandler: (error: { code?: string; message: string }) => void = () => {};
  private closeHandler: () => void = () => {};
  private lastRequest: { contextId: string; plainText: string; more: boolean; hadMarkup: boolean } | null = null;
  private retriedPlainText = false;

  public constructor(private readonly socket: WebSocket, connectionId: string | null = null) {
    this.connectionId = connectionId;
    socket.on('message', (data, isBinary) => { if (!isBinary) this.handle(data.toString('utf8')); });
    socket.on('close', () => this.closeHandler());
    socket.on('error', (error) => this.errorHandler({ message: error.message }));
  }

  public send(contextId: string, preparation: SpeechPreparation, more: boolean): void {
    const action = (): void => {
      this.activeContextId = contextId;
      const rendered = renderDeepdubText(preparation);
      this.lastRequest = { contextId, plainText: escapeXml(preparation.spokenText), more, hadMarkup: rendered !== escapeXml(preparation.spokenText) };
      this.retriedPlainText = false;
      if (preparation.spokenText) this.sendJson({ action: 'stream-text', data: { text: rendered } });
      if (!more) this.sendJson({ action: 'end-stream' });
    };
    if (this.cancelPending) this.deferred.push(action); else action();
  }

  public cancel(contextId: string): void {
    if (this.activeContextId !== contextId) return;
    this.activeContextId = null;
    this.cancelPending = true;
    this.sendJson({ action: 'cancel' });
  }
  public close(): void { if (this.socket.readyState === WebSocket.OPEN || this.socket.readyState === WebSocket.CONNECTING) this.socket.close(); }
  public onAudio(handler: (event: { contextId: string; data: string }) => void): void { this.audioHandler = handler; }
  public onDone(handler: (contextId: string) => void): void { this.doneHandler = handler; }
  public onError(handler: (error: { code?: string; message: string }) => void): void { this.errorHandler = handler; }
  public onClose(handler: () => void): void { this.closeHandler = handler; }
  public sessionId(): string | null { return this.connectionId; }

  private sendJson(value: Record<string, unknown>): void { if (this.socket.readyState === WebSocket.OPEN) this.socket.send(JSON.stringify(value)); }
  private releaseCancelled(): void {
    this.cancelPending = false;
    const queued = this.deferred.splice(0);
    for (const action of queued) action();
  }
  private handle(raw: string): void {
    const message = parseJsonObject(raw);
    if (!message) return;
    if (readString(message, 'action') === 'status') {
      this.connectionId = readString(message, 'connectionId') ?? this.connectionId;
      if (readString(message, 'message') === 'cancelled') this.releaseCancelled();
      return;
    }
    if (readString(message, 'action') === 'error' || typeof message['error'] === 'string') {
      if (this.activeContextId && this.lastRequest?.contextId === this.activeContextId && this.lastRequest.hadMarkup && !this.retriedPlainText) {
        this.retriedPlainText = true;
        this.sendJson({ action: 'stream-text', data: { text: this.lastRequest.plainText } });
        if (!this.lastRequest.more) this.sendJson({ action: 'end-stream' });
        return;
      }
      this.errorHandler({ message: readString(message, 'message') ?? readString(message, 'error') ?? 'Deepdub TTS error' });
      return;
    }
    if (message['isCancelled'] === true) {
      this.releaseCancelled();
      return;
    }
    const contextId = this.activeContextId;
    const data = readString(message, 'data');
    if (contextId && data) this.audioHandler({ contextId, data });
    if (contextId && message['isFinished'] === true && message['isFinal'] === true) {
      this.activeContextId = null;
      this.doneHandler(contextId);
    }
  }
}

export async function connectDeepdub(options: DeepdubConnectOptions): Promise<{ socket: WebSocket; session: DeepdubTtsSession }> {
  const timeoutMs = options.connectTimeoutMs ?? 10_000;
  const socket = new WebSocket(options.url, { headers: { 'x-api-key': options.apiKey }, handshakeTimeout: timeoutMs });
  const connectionId = await new Promise<string | null>((resolve, reject) => {
    const timer = setTimeout(() => { socket.terminate(); reject(new Error('Timed out connecting to Deepdub')); }, timeoutMs);
    const cleanup = (): void => { clearTimeout(timer); socket.off('error', onError); socket.off('message', onMessage); };
    const onError = (error: Error): void => { cleanup(); reject(error); };
    const onMessage = (data: WebSocket.RawData, isBinary: boolean): void => {
      if (isBinary) return;
      const message = parseJsonObject(data.toString('utf8'));
      if (message && readString(message, 'action') === 'status' && readString(message, 'message') === 'connected') { cleanup(); resolve(readString(message, 'connectionId') ?? null); }
    };
    socket.once('error', onError);
    socket.on('message', onMessage);
  });
  socket.send(JSON.stringify({ action: 'stream-config', data: { model: options.model, voicePromptId: options.voiceId, locale: options.locale, format: 'mulaw', sampleRate: 8000, realtime: true, cleanAudio: true, enableLogging: options.enableLogging, firstAudioTimeout: options.firstAudioTimeoutMs, ...(options.targetGender ? { targetGender: options.targetGender } : {}) } }));
  return { socket, session: new DeepdubTtsSession(socket, connectionId) };
}

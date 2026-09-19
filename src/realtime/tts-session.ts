import type { SpeechPreparation } from '../hebrew/types.js';
import type { CartesiaSocket } from './cartesia-connection.js';
import { buildTtsCancel, buildTtsChunk } from './cartesia-protocol.js';
import { parseJsonObject, readString } from './protocol.js';

export interface SpeechSynthesisSession {
  send(contextId: string, preparation: SpeechPreparation, more: boolean): void;
  cancel(contextId: string): void;
  close(): void;
  onAudio(handler: (event: { contextId: string; data: string }) => void): void;
  onDone(handler: (contextId: string) => void): void;
  onError(handler: (error: { code?: string; message: string }) => void): void;
  onClose(handler: () => void): void;
  sessionId(): string | null;
}

export class CartesiaTtsSession implements SpeechSynthesisSession {
  private audioHandler: (event: { contextId: string; data: string }) => void = () => {};
  private doneHandler: (contextId: string) => void = () => {};
  private errorHandler: (error: { code?: string; message: string }) => void = () => {};

  public constructor(
    private readonly socket: CartesiaSocket,
    private readonly options: { model: string; voiceId: string; language?: string },
  ) {
    socket.onMessage((raw) => this.handle(raw));
    socket.onError((error) => this.errorHandler({ message: error.message }));
  }

  public send(contextId: string, preparation: SpeechPreparation, more: boolean): void {
    this.socket.sendText(JSON.stringify(buildTtsChunk({ model: this.options.model, voiceId: this.options.voiceId, contextId, transcript: preparation.spokenText, language: this.options.language, continue: more })));
  }
  public cancel(contextId: string): void { this.socket.sendText(JSON.stringify(buildTtsCancel(contextId))); }
  public close(): void { this.socket.close(); }
  public onAudio(handler: (event: { contextId: string; data: string }) => void): void { this.audioHandler = handler; }
  public onDone(handler: (contextId: string) => void): void { this.doneHandler = handler; }
  public onError(handler: (error: { code?: string; message: string }) => void): void { this.errorHandler = handler; }
  public onClose(handler: () => void): void { this.socket.onClose(handler); }
  public sessionId(): string | null { return null; }

  private handle(raw: string): void {
    const message = parseJsonObject(raw);
    if (!message) return;
    const type = readString(message, 'type');
    if (type === 'chunk') {
      const contextId = readString(message, 'context_id');
      const data = readString(message, 'data');
      if (contextId && data) this.audioHandler({ contextId, data });
    } else if (type === 'done') {
      const contextId = readString(message, 'context_id');
      if (contextId) this.doneHandler(contextId);
    } else if (type === 'error') {
      const code = readString(message, 'error_code');
      this.errorHandler({ ...(code ? { code } : {}), message: readString(message, 'message') ?? readString(message, 'title') ?? 'Cartesia TTS error' });
    }
  }
}

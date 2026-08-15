import WebSocket from 'ws';
import { buildSttUrl, buildTtsUrl } from './cartesia-protocol.js';

/**
 * Cartesia socket setup.
 *
 * The API key travels only in the `X-API-Key` handshake header — never in a URL, so it
 * cannot leak through a logged endpoint. Errors report status and message text without
 * the credential.
 */

export interface CartesiaConnectOptions {
  apiKey: string;
  baseUrl: string;
  version: string;
  connectTimeoutMs?: number;
}

/**
 * Duplex channel that can carry binary audio as well as text.
 *
 * The bridge's own `MessageChannel` is text-only, which suits the JSON protocols of the
 * other providers but not Cartesia STT: it takes raw mu-law bytes as binary frames.
 */
export interface CartesiaSocket {
  sendText(payload: string): void;
  sendBinary(payload: Buffer): void;
  close(): void;
  onMessage(handler: (raw: string) => void): void;
  onClose(handler: () => void): void;
  onError(handler: (error: Error) => void): void;
}

/** Buffers anything arriving before the bridge attaches, so no transcript is dropped. */
export function cartesiaSocket(socket: WebSocket): CartesiaSocket {
  const buffered: string[] = [];
  let messageHandler: ((raw: string) => void) | null = null;
  let closeHandler: (() => void) | null = null;
  let errorHandler: ((error: Error) => void) | null = null;
  let closedEarly = false;
  let earlyError: Error | null = null;

  socket.on('message', (data: WebSocket.RawData, isBinary: boolean) => {
    // Both Cartesia sockets reply in JSON text; audio comes back base64 inside it.
    if (isBinary) {
      return;
    }
    const raw = data.toString('utf8');
    if (messageHandler) {
      messageHandler(raw);
    } else {
      buffered.push(raw);
    }
  });
  socket.on('close', () => {
    if (closeHandler) {
      closeHandler();
    } else {
      closedEarly = true;
    }
  });
  socket.on('error', (error: Error) => {
    if (errorHandler) {
      errorHandler(error);
    } else {
      earlyError = error;
    }
  });

  const open = (): boolean => socket.readyState === WebSocket.OPEN;

  return {
    sendText(payload: string): void {
      if (open()) {
        socket.send(payload);
      }
    },
    sendBinary(payload: Buffer): void {
      if (open()) {
        socket.send(payload, { binary: true });
      }
    },
    close(): void {
      if (open() || socket.readyState === WebSocket.CONNECTING) {
        socket.close();
      }
    },
    onMessage(handler: (raw: string) => void): void {
      messageHandler = handler;
      while (buffered.length > 0) {
        handler(buffered.shift()!);
      }
    },
    onClose(handler: () => void): void {
      closeHandler = handler;
      if (closedEarly) {
        closedEarly = false;
        handler();
      }
    },
    onError(handler: (error: Error) => void): void {
      errorHandler = handler;
      if (earlyError) {
        const error = earlyError;
        earlyError = null;
        handler(error);
      }
    },
  };
}

async function connect(url: string, apiKey: string, connectTimeoutMs: number, label: string): Promise<WebSocket> {
  const socket = new WebSocket(url, {
    headers: { 'X-API-Key': apiKey },
    handshakeTimeout: connectTimeoutMs,
  });

  return await new Promise<WebSocket>((resolve, reject) => {
    const timer = setTimeout(() => {
      cleanup();
      socket.terminate();
      reject(new Error(`Timed out connecting to the Cartesia ${label} API`));
    }, connectTimeoutMs);

    const onOpen = (): void => {
      cleanup();
      resolve(socket);
    };
    const onError = (error: Error): void => {
      cleanup();
      socket.terminate();
      reject(error);
    };
    function cleanup(): void {
      clearTimeout(timer);
      socket.off('open', onOpen);
      socket.off('error', onError);
    }

    socket.once('open', onOpen);
    socket.once('error', onError);
  });
}

export interface SttConnectOptions extends CartesiaConnectOptions {
  model: string;
  language?: string | undefined;
}

export async function connectCartesiaStt(options: SttConnectOptions): Promise<WebSocket> {
  const url = buildSttUrl({
    baseUrl: options.baseUrl,
    model: options.model,
    version: options.version,
    language: options.language,
  });
  return await connect(url, options.apiKey, options.connectTimeoutMs ?? 10_000, 'speech-to-text');
}

export async function connectCartesiaTts(options: CartesiaConnectOptions): Promise<WebSocket> {
  const url = buildTtsUrl({ baseUrl: options.baseUrl, version: options.version });
  return await connect(url, options.apiKey, options.connectTimeoutMs ?? 10_000, 'text-to-speech');
}

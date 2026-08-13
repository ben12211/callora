import WebSocket from 'ws';

export interface OpenAiRealtimeConnectOptions {
  apiKey: string;
  /** Base realtime WebSocket URL, without the model query parameter. */
  url: string;
  model: string;
  connectTimeoutMs?: number;
}

/**
 * Opens one OpenAI Realtime WebSocket (GA interface: bearer auth, no beta header) and
 * resolves once the socket is open, or rejects on error/timeout without leaking a socket.
 */
export async function connectOpenAiRealtime(options: OpenAiRealtimeConnectOptions): Promise<WebSocket> {
  const { apiKey, url, model, connectTimeoutMs = 10_000 } = options;
  const target = new URL(url);
  target.searchParams.set('model', model);

  const socket = new WebSocket(target.toString(), {
    headers: { Authorization: `Bearer ${apiKey}` },
    handshakeTimeout: connectTimeoutMs,
  });

  return await new Promise<WebSocket>((resolve, reject) => {
    const timer = setTimeout(() => {
      cleanup();
      socket.terminate();
      reject(new Error('Timed out connecting to the OpenAI Realtime API'));
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

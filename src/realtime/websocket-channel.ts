import WebSocket from 'ws';
import type { MessageChannel } from './call-leg.js';

/**
 * Adapts a `ws` WebSocket to the bridge's text channel interface.
 *
 * Listeners are attached immediately and anything that arrives before the bridge
 * registers its handlers is buffered, so no caller audio is dropped while the paired
 * OpenAI Realtime session is still being established.
 */
export function websocketChannel(socket: WebSocket): MessageChannel {
  const buffered: string[] = [];
  let messageHandler: ((raw: string) => void) | null = null;
  let closeHandler: (() => void) | null = null;
  let errorHandler: ((error: Error) => void) | null = null;
  let closedEarly = false;
  let earlyError: Error | null = null;

  socket.on('message', (data: WebSocket.RawData, isBinary: boolean) => {
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

  return {
    send(payload: string): void {
      if (socket.readyState === WebSocket.OPEN) {
        socket.send(payload);
      }
    },
    close(): void {
      if (socket.readyState === WebSocket.OPEN || socket.readyState === WebSocket.CONNECTING) {
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

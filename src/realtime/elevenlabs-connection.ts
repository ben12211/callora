import WebSocket from 'ws';
import { SIGNED_URL_PATH } from './elevenlabs-protocol.js';

export interface ElevenLabsConnectOptions {
  apiKey: string;
  agentId: string;
  /** API origin, without a trailing slash. */
  baseUrl: string;
  connectTimeoutMs?: number;
  /** Injectable so tests never reach the ElevenLabs API. */
  fetchImpl?: typeof fetch;
}

/**
 * Exchanges the account API key for a short-lived signed WebSocket URL.
 *
 * The key is sent only in this server-to-server request header and never appears in the
 * socket URL, in Twilio traffic, or in a log line. Errors deliberately report the HTTP
 * status only, so a failure cannot echo the key or the signed token back into logs.
 */
export async function fetchSignedUrl(options: ElevenLabsConnectOptions): Promise<string> {
  const { apiKey, agentId, baseUrl, fetchImpl = fetch } = options;
  const target = new URL(`${baseUrl}${SIGNED_URL_PATH}`);
  target.searchParams.set('agent_id', agentId);

  const response = await fetchImpl(target.toString(), { headers: { 'xi-api-key': apiKey } });
  if (!response.ok) {
    throw new Error(`ElevenLabs rejected the signed URL request with status ${response.status}`);
  }

  const body: unknown = await response.json();
  const signedUrl =
    typeof body === 'object' && body !== null && 'signed_url' in body
      ? (body as { signed_url: unknown }).signed_url
      : undefined;

  if (typeof signedUrl !== 'string' || signedUrl.length === 0) {
    throw new Error('ElevenLabs did not return a signed URL');
  }
  return signedUrl;
}

/**
 * Opens one ElevenLabs Agents WebSocket and resolves once it is open, or rejects on
 * error/timeout without leaking a socket.
 */
export async function connectElevenLabsAgent(options: ElevenLabsConnectOptions): Promise<WebSocket> {
  const { connectTimeoutMs = 10_000 } = options;
  const signedUrl = await fetchSignedUrl(options);
  const socket = new WebSocket(signedUrl, { handshakeTimeout: connectTimeoutMs });

  return await new Promise<WebSocket>((resolve, reject) => {
    const timer = setTimeout(() => {
      cleanup();
      socket.terminate();
      reject(new Error('Timed out connecting to the ElevenLabs Agents API'));
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

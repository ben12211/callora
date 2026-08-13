import type { FastifyInstance, FastifyRequest } from 'fastify';
import twilio from 'twilio';
import type WebSocket from 'ws';
import type { AppConfig } from '../config.js';
import type { DataStore } from '../db/store.js';
import { MediaStreamBridge, type MessageChannel } from '../realtime/bridge.js';
import { connectOpenAiRealtime } from '../realtime/openai-connection.js';
import { parseJsonObject, readObject, readString } from '../realtime/protocol.js';
import { websocketChannel } from '../realtime/websocket-channel.js';
import { createCallTerminator, type CallTerminator } from '../telephony/call-terminator.js';
import { verifyStreamToken, type StreamTokenPayload } from './stream-token.js';

export const MEDIA_STREAM_PATH = '/webhooks/twilio/media';

/** Hard ceiling for a single bridged call, so a stuck stream cannot live forever. */
const MAX_CALL_DURATION_MS = 60 * 60 * 1000;
const STREAM_AUTH_TIMEOUT_MS = 5_000;

interface MediaStreamDependencies {
  config: AppConfig;
  store: DataStore;
  /** Overridable so tests never reach the Twilio REST API. */
  callTerminator?: CallTerminator;
}

function validWebSocketSignature(request: FastifyRequest, config: AppConfig): boolean {
  const signatureHeader = request.headers['x-twilio-signature'];
  const signature = Array.isArray(signatureHeader) ? signatureHeader[0] : signatureHeader;
  if (!signature) {
    return false;
  }

  const requestPath = request.raw.url ?? request.url;
  const webSocketBaseUrl = config.publicBaseUrl.replace(/^http/, 'ws');
  return twilio.validateRequest(config.twilioAuthToken, signature, `${webSocketBaseUrl}${requestPath}`, {});
}

export function streamClaimsFromStartMessage(
  raw: string,
  secret: string,
): StreamTokenPayload | null {
  const message = parseJsonObject(raw);
  if (!message || readString(message, 'event') !== 'start') {
    return null;
  }

  const start = readObject(message, 'start');
  const customParameters = start ? readObject(start, 'customParameters') : undefined;
  const token = customParameters ? readString(customParameters, 'token') : undefined;
  const startedCallSid = start ? readString(start, 'callSid') : undefined;
  const claims = token ? verifyStreamToken(secret, token) : null;

  return claims && startedCallSid === claims.callSid ? claims : null;
}

function replayBufferedMessages(channel: MessageChannel, buffered: string[]): MessageChannel {
  return {
    send: (payload) => channel.send(payload),
    close: () => channel.close(),
    onMessage: (handler) => {
      const replay = buffered.splice(0);
      channel.onMessage(handler);
      for (const raw of replay) {
        handler(raw);
      }
    },
    onClose: (handler) => channel.onClose(handler),
    onError: (handler) => channel.onError(handler),
  };
}

async function authorizeTwilioStream(
  channel: MessageChannel,
  secret: string,
): Promise<{ channel: MessageChannel; claims: StreamTokenPayload }> {
  return new Promise((resolve, reject) => {
    const buffered: string[] = [];
    let settled = false;

    const fail = (error: Error): void => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      reject(error);
    };
    const timer = setTimeout(() => fail(new Error('Timed out waiting for the Twilio start event')), STREAM_AUTH_TIMEOUT_MS);
    timer.unref?.();

    channel.onMessage((raw) => {
      buffered.push(raw);
      if (settled) return;

      const message = parseJsonObject(raw);
      const event = message ? readString(message, 'event') : undefined;
      if (event === 'connected') return;
      if (event !== 'start') {
        fail(new Error('Received media stream data before authorization'));
        return;
      }

      const claims = streamClaimsFromStartMessage(raw, secret);
      if (!claims) {
        fail(new Error('Invalid media stream token'));
        return;
      }

      settled = true;
      clearTimeout(timer);
      resolve({ channel: replayBufferedMessages(channel, buffered), claims });
    });
    channel.onClose(() => fail(new Error('Media stream closed before authorization')));
    channel.onError((error) => fail(error));
  });
}

export function registerMediaStreamRoute(app: FastifyInstance, dependencies: MediaStreamDependencies): void {
  const { config } = dependencies;
  const resolved: Required<Pick<MediaStreamDependencies, 'callTerminator'>> & MediaStreamDependencies = {
    ...dependencies,
    callTerminator: dependencies.callTerminator ?? createCallTerminator(config),
  };

  app.get(
    MEDIA_STREAM_PATH,
    {
      websocket: true,
      preValidation: async (request, reply) => {
        if (!validWebSocketSignature(request, config)) {
          app.log.warn({ ip: request.ip }, 'Rejected invalid Twilio media stream signature');
          await reply.code(403).send({ error: 'Invalid Twilio signature' });
        }
      },
    },
    (socket: WebSocket, request) => {
      void startAuthorizedBridge(app, resolved, socket, request);
    },
  );
}

async function startAuthorizedBridge(
  app: FastifyInstance,
  dependencies: MediaStreamDependencies,
  socket: WebSocket,
  request: FastifyRequest,
): Promise<void> {
  let authorized: Awaited<ReturnType<typeof authorizeTwilioStream>>;
  try {
    authorized = await authorizeTwilioStream(websocketChannel(socket), dependencies.config.twilioAuthToken);
  } catch (error) {
    app.log.warn(
      { ip: request.ip, error: error instanceof Error ? error.message : 'unknown error' },
      'Rejected unauthorized Twilio media stream',
    );
    socket.close(1008, 'unauthorized');
    return;
  }

  const { claims } = authorized;
  try {
    await openBridge(
      app,
      dependencies,
      socket,
      authorized.channel,
      claims.businessId,
      claims.callSid,
    );
  } catch (error) {
    app.log.error(
      {
        businessId: claims.businessId,
        callSid: claims.callSid,
        error: error instanceof Error ? error.message : 'unknown error',
      },
      'Failed to start the realtime call bridge',
    );
    socket.close(1011, 'bridge failed');
  }
}

async function openBridge(
  app: FastifyInstance,
  dependencies: MediaStreamDependencies,
  socket: WebSocket,
  twilioChannel: MessageChannel,
  businessId: string,
  callSid: string,
): Promise<void> {
  const { config, store } = dependencies;

  const agent = await store.getAgentConfig(businessId);
  if (!agent || !agent.enabled) {
    app.log.warn({ businessId, callSid }, 'No enabled agent configuration for the resolved business');
    socket.close(1011, 'agent unavailable');
    return;
  }

  const call = await store.getCallByTwilioSid(callSid);
  if (call && call.businessId !== businessId) {
    app.log.warn({ businessId, callSid }, 'Call belongs to another business; refusing to bridge');
    socket.close(1008, 'unauthorized');
    return;
  }

  const openaiSocket = await connectOpenAiRealtime({
    apiKey: config.openaiApiKey,
    url: config.openaiRealtimeUrl,
    model: agent.realtimeModel,
  });
  app.log.info({ businessId, callSid, model: agent.realtimeModel }, 'OpenAI realtime connection opened');

  let persistedStreamSid: string | null = null;
  let persistedSessionId: string | null = null;

  const bridge = new MediaStreamBridge({
    twilio: twilioChannel,
    openai: websocketChannel(openaiSocket),
    agent,
    businessId,
    callSid,
    callerNumber: call?.fromNumber ?? null,
    logger: app.log,
    // The CallSid is the one this stream was authorized for; the model never supplies it.
    endCall: dependencies.callTerminator
      ? async (): Promise<void> => {
          await dependencies.callTerminator!.endCall(callSid);
        }
      : undefined,
    onIdentifiers: ({ streamSid, openaiSessionId }) => {
      if (streamSid === persistedStreamSid && openaiSessionId === persistedSessionId) {
        return;
      }
      persistedStreamSid = streamSid ?? persistedStreamSid;
      persistedSessionId = openaiSessionId ?? persistedSessionId;
      void store
        .attachRealtimeSession({
          businessId,
          twilioCallSid: callSid,
          twilioStreamSid: persistedStreamSid,
          openaiSessionId: persistedSessionId,
        })
        .catch((error: unknown) => {
          app.log.error(
            { businessId, callSid, error: error instanceof Error ? error.message : 'unknown error' },
            'Failed to persist realtime identifiers',
          );
        });
    },
  });

  const maxDurationTimer = setTimeout(() => bridge.close('max-duration'), MAX_CALL_DURATION_MS);
  maxDurationTimer.unref?.();
  socket.once('close', () => clearTimeout(maxDurationTimer));
  openaiSocket.once('close', () => clearTimeout(maxDurationTimer));

  bridge.start();
}

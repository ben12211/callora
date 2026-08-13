import type { FastifyInstance, FastifyRequest } from 'fastify';
import type WebSocket from 'ws';
import type { AppConfig } from '../config.js';
import type { DataStore } from '../db/store.js';
import { MediaStreamBridge } from '../realtime/bridge.js';
import { connectOpenAiRealtime } from '../realtime/openai-connection.js';
import { websocketChannel } from '../realtime/websocket-channel.js';
import { verifyStreamToken } from './stream-token.js';

export const MEDIA_STREAM_PATH = '/webhooks/twilio/media';

/** Hard ceiling for a single bridged call, so a stuck stream cannot live forever. */
const MAX_CALL_DURATION_MS = 60 * 60 * 1000;

interface MediaStreamDependencies {
  config: AppConfig;
  store: DataStore;
}

function tokenFromQuery(request: FastifyRequest): string | null {
  const query = request.query;
  if (typeof query !== 'object' || query === null) {
    return null;
  }
  const value = (query as Record<string, unknown>)['token'];
  return typeof value === 'string' && value.length > 0 ? value : null;
}

export function registerMediaStreamRoute(app: FastifyInstance, dependencies: MediaStreamDependencies): void {
  const { config } = dependencies;

  app.get(
    MEDIA_STREAM_PATH,
    {
      websocket: true,
      preValidation: async (request, reply) => {
        const token = tokenFromQuery(request);
        if (!token || !verifyStreamToken(config.twilioAuthToken, token)) {
          app.log.warn({ ip: request.ip }, 'Rejected unauthorized Twilio media stream handshake');
          await reply.code(403).send({ error: 'Invalid media stream token' });
        }
      },
    },
    (socket: WebSocket, request) => {
      const token = tokenFromQuery(request);
      const claims = token ? verifyStreamToken(config.twilioAuthToken, token) : null;
      if (!claims) {
        socket.close(1008, 'unauthorized');
        return;
      }

      void openBridge(app, dependencies, socket, claims.businessId, claims.callSid).catch((error: unknown) => {
        app.log.error(
          {
            businessId: claims.businessId,
            callSid: claims.callSid,
            error: error instanceof Error ? error.message : 'unknown error',
          },
          'Failed to start the realtime call bridge',
        );
        socket.close(1011, 'bridge failed');
      });
    },
  );
}

async function openBridge(
  app: FastifyInstance,
  dependencies: MediaStreamDependencies,
  socket: WebSocket,
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
    twilio: websocketChannel(socket),
    openai: websocketChannel(openaiSocket),
    agent,
    businessId,
    callSid,
    callerNumber: call?.fromNumber ?? null,
    logger: app.log,
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

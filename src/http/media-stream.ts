import type { FastifyInstance, FastifyRequest } from 'fastify';
import twilio from 'twilio';
import type WebSocket from 'ws';
import type { AppConfig } from '../config.js';
import type { DataStore } from '../db/store.js';
import type { MetricsRegistry } from '../platform/metrics.js';
import type { PlatformRuntime } from '../platform/settings.js';
import { MediaStreamBridge, type MessageChannel } from '../realtime/bridge.js';
import { CartesiaBridge } from '../realtime/cartesia-bridge.js';
import { cartesiaSocket, connectCartesiaStt, connectCartesiaTts } from '../realtime/cartesia-connection.js';
import { cartesiaLanguage } from '../realtime/cartesia-protocol.js';
import { ElevenLabsBridge } from '../realtime/elevenlabs-bridge.js';
import { connectElevenLabsAgent } from '../realtime/elevenlabs-connection.js';
import { connectOpenAiRealtime } from '../realtime/openai-connection.js';
import { parseJsonObject, readObject, readString } from '../realtime/protocol.js';
import { websocketChannel } from '../realtime/websocket-channel.js';
import type { CallRegistry } from '../telephony/call-registry.js';
import { createCallTerminator, type CallTerminator } from '../telephony/call-terminator.js';
import { verifyStreamToken, type StreamTokenPayload } from './stream-token.js';

export const MEDIA_STREAM_PATH = '/webhooks/twilio/media';

/** Hard ceiling for a single bridged call, so a stuck stream cannot live forever. */
const MAX_CALL_DURATION_MS = 60 * 60 * 1000;
const STREAM_AUTH_TIMEOUT_MS = 5_000;

interface MediaStreamDependencies {
  config: AppConfig;
  store: DataStore;
  /** Live provider credentials, so a key saved in the dashboard applies to the next call. */
  platform: PlatformRuntime;
  /** Overridable so tests never reach the Twilio REST API. */
  callTerminator?: CallTerminator;
  /** Live calls on this instance, so a deploy can drain instead of cutting them off. */
  registry?: CallRegistry;
  /** Call-path counters; absent simply records nothing. */
  metrics?: MetricsRegistry;
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
  secret: string | readonly string[],
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
  secret: string | readonly string[],
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
    authorized = await authorizeTwilioStream(websocketChannel(socket), dependencies.config.streamTokenSecrets);
  } catch (error) {
    app.log.warn(
      { ip: request.ip, error: error instanceof Error ? error.message : 'unknown error' },
      'Rejected unauthorized Twilio media stream',
    );
    socket.close(1008, 'unauthorized');
    return;
  }

  const { claims } = authorized;
  if (dependencies.registry?.isDraining) {
    // This instance is going away. Refusing the stream is better than accepting a call
    // that would be cut off part-way through the drain window.
    app.log.warn(
      { businessId: claims.businessId, callSid: claims.callSid },
      'Refusing a new media stream while shutting down',
    );
    await fallbackToGreeting(app, dependencies, claims.businessId, claims.callSid, 'shutting-down');
    socket.close(1012, 'shutting down');
    return;
  }

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
    // A <Connect><Stream> call whose stream just closes hears nothing at all. Hand the
    // call back to TwiML so the caller gets the business's greeting instead of silence.
    await fallbackToGreeting(app, dependencies, claims.businessId, claims.callSid, 'bridge-failed');
    socket.close(1011, 'bridge failed');
  }
}

/** Last line before dead air: speak the tenant's static greeting and hang up. */
export async function fallbackToGreeting(
  app: FastifyInstance,
  dependencies: MediaStreamDependencies,
  businessId: string,
  callSid: string,
  reason: string,
): Promise<void> {
  const terminator = dependencies.callTerminator;
  if (!terminator) {
    return;
  }
  try {
    const business = await dependencies.store.getBusinessById(businessId);
    const line = business?.greeting?.trim();
    if (!line) {
      app.log.warn({ businessId, callSid, reason }, 'No greeting to fall back to; hanging up');
      await terminator.endCall(callSid);
      return;
    }
    await terminator.sayAndHangUp(callSid, line);
    dependencies.metrics?.greetingFallback(reason);
    app.log.info({ businessId, callSid, reason }, 'Answered with the static greeting after the realtime path failed');
  } catch (error) {
    app.log.error(
      { businessId, callSid, reason, error: error instanceof Error ? error.message : 'unknown error' },
      'Failed to fall back to the static greeting',
    );
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
  const { store } = dependencies;
  // Settings saved on another instance, or written straight into the database, reach this
  // call rather than the next restart.
  await dependencies.platform.refreshIfStale();
  // Read once per call: the credentials in force when this call was answered are the ones
  // it runs on, even if an operator saves new ones while it is still connected.
  const providers = dependencies.platform.providers();

  const agent = await store.getAgentConfig(businessId);
  if (!agent || !agent.enabled) {
    app.log.warn({ businessId, callSid }, 'No enabled agent configuration for the resolved business');
    await fallbackToGreeting(app, dependencies, businessId, callSid, 'agent-unavailable');
    socket.close(1011, 'agent unavailable');
    return;
  }

  const call = await store.getCallByTwilioSid(callSid);
  if (call && call.businessId !== businessId) {
    app.log.warn({ businessId, callSid }, 'Call belongs to another business; refusing to bridge');
    socket.close(1008, 'unauthorized');
    return;
  }

  let persistedStreamSid: string | null = null;
  let persistedSessionId: string | null = null;

  /**
   * Shared by every provider: the call row records which backend ran it alongside that
   * backend's own session identifier.
   */
  const persistIdentifiers = (
    streamSid: string | null,
    sessionId: string | null,
    provider: string,
  ): void => {
    if (streamSid === persistedStreamSid && sessionId === persistedSessionId) {
      return;
    }
    persistedStreamSid = streamSid ?? persistedStreamSid;
    persistedSessionId = sessionId ?? persistedSessionId;
    void store
      .attachRealtimeSession({
        businessId,
        twilioCallSid: callSid,
        twilioStreamSid: persistedStreamSid,
        providerSessionId: persistedSessionId,
        provider,
      })
      .catch((error: unknown) => {
        app.log.error(
          { businessId, callSid, error: error instanceof Error ? error.message : 'unknown error' },
          'Failed to persist realtime identifiers',
        );
      });
  };

  /**
   * Persists one conversation turn.
   *
   * Best effort and fire-and-forget, exactly like the identifier writes above: a
   * transcript that cannot be stored must never interrupt the call it describes. Turns
   * are numbered in arrival order, which is what makes the stored conversation readable.
   */
  let turnNumber = 0;
  const persistTurn = (turn: { speaker: 'caller' | 'agent'; content: string }): void => {
    const callId = call?.id;
    if (!callId) {
      return;
    }
    turnNumber += 1;
    void store
      .appendTranscriptTurn({
        callId,
        businessId,
        speaker: turn.speaker,
        content: turn.content,
        turn: turnNumber,
      })
      .catch((error: unknown) => {
        app.log.warn(
          { businessId, callSid, error: error instanceof Error ? error.message : 'unknown error' },
          'Failed to persist a transcript turn',
        );
      });
  };

  // The CallSid is the one this stream was authorized for; the model never supplies it.
  const endCall = dependencies.callTerminator
    ? async (): Promise<void> => {
        await dependencies.callTerminator!.endCall(callSid);
      }
    : undefined;

  /**
   * Registers the live call so a shutdown can wait for it, and unregisters it however it
   * ends. Without this a deploy closes the server underneath every conversation.
   */
  const track = (bridgeClose: (reason: string) => void, provider: string): void => {
    const startedAt = Date.now();
    dependencies.metrics?.callStarted(provider);
    socket.once('close', (code: number) => {
      dependencies.metrics?.callEnded(provider, String(code), (Date.now() - startedAt) / 1000);
    });

    const registry = dependencies.registry;
    if (!registry) {
      return;
    }
    registry.add({
      businessId,
      callSid,
      provider,
      startedAt,
      close: (reason) => bridgeClose(reason),
    });
    socket.once('close', () => registry.remove(callSid));
  };

  /** One ceiling per call, cleared by whichever side disconnects first. */
  const guardDuration = (close: () => void, providerSocket: WebSocket): void => {
    const maxDurationTimer = setTimeout(close, MAX_CALL_DURATION_MS);
    maxDurationTimer.unref?.();
    socket.once('close', () => clearTimeout(maxDurationTimer));
    providerSocket.once('close', () => clearTimeout(maxDurationTimer));
  };

  // The business chose the provider; the platform only supplies the credentials.
  const provider = agent.voiceProvider;

  if (provider === 'cartesia') {
    const credentials = providers.cartesia;
    if (!credentials) {
      app.log.error({ businessId, callSid, provider }, 'No platform credentials for the selected provider');
      await fallbackToGreeting(app, dependencies, businessId, callSid, 'provider-unavailable');
      socket.close(1011, 'provider unavailable');
      return;
    }
    // A Cartesia agent stores its Sonic voice UUID in `voice`; the environment default
    // covers agents configured before a voice was chosen.
    const voiceId = agent.voice.trim() || credentials.defaultVoiceId;
    if (!voiceId) {
      app.log.error({ businessId, callSid }, 'The Cartesia agent has no voice id and no platform default');
      await fallbackToGreeting(app, dependencies, businessId, callSid, 'voice-unavailable');
      socket.close(1011, 'provider unavailable');
      return;
    }
    const language = cartesiaLanguage(agent);
    const connectOptions = {
      apiKey: credentials.apiKey,
      baseUrl: credentials.wsBaseUrl,
      version: credentials.version,
    };
    // Both sockets are opened before any audio flows, so the first caller frame is never
    // dropped waiting for a half-built pipeline.
    const [sttSocket, ttsSocket] = await Promise.all([
      connectCartesiaStt({ ...connectOptions, model: credentials.sttModel, language }),
      connectCartesiaTts(connectOptions),
    ]);
    // The reasoning model is per agent; Cartesia's own models stay platform-level.
    const llmModel = agent.realtimeModel.trim() || credentials.textLlmModel;
    // Models and the voice id are safe to log; the API key never is.
    app.log.info(
      {
        businessId,
        callSid,
        sttModel: credentials.sttModel,
        ttsModel: credentials.ttsModel,
        llmModel,
        voiceId,
        language: language ?? null,
      },
      'Cartesia pipeline opened',
    );

    const bridge = new CartesiaBridge({
      twilio: twilioChannel,
      stt: cartesiaSocket(sttSocket),
      tts: cartesiaSocket(ttsSocket),
      agent,
      businessId,
      callSid,
      callId: call?.id ?? null,
      callerNumber: call?.fromNumber ?? null,
      ttsModel: credentials.ttsModel,
      voiceId,
      llm: { baseUrl: credentials.textLlmBaseUrl, apiKey: credentials.textLlmApiKey, model: llmModel },
      logger: app.log,
      endCall,
      ...(dependencies.metrics ? { metrics: dependencies.metrics } : {}),
      onTranscript: persistTurn,
      onIdentifiers: ({ streamSid, sessionId }) => persistIdentifiers(streamSid, sessionId, provider),
    });

    guardDuration(() => bridge.close('max-duration'), sttSocket);
    ttsSocket.once('close', () => bridge.close('tts-closed'));
    track((reason) => bridge.close(reason), 'cartesia');
    bridge.start();
    return;
  }

  if (provider === 'elevenlabs') {
    const credentials = providers.elevenlabs;
    if (!credentials) {
      app.log.error({ businessId, callSid, provider }, 'No platform credentials for the selected provider');
      await fallbackToGreeting(app, dependencies, businessId, callSid, 'provider-unavailable');
      socket.close(1011, 'provider unavailable');
      return;
    }
    // A business that owns an agent runs on it; the rest share the platform agent.
    const agentId = agent.elevenLabsAgentId.trim() || credentials.agentId;
    const elevenLabsSocket = await connectElevenLabsAgent({
      apiKey: credentials.apiKey,
      agentId,
      baseUrl: credentials.apiBaseUrl,
    });
    // The agent id is safe to log; the API key and the signed URL never are.
    app.log.info({ businessId, callSid, agentId }, 'ElevenLabs conversation opened');

    const bridge = new ElevenLabsBridge({
      twilio: twilioChannel,
      elevenlabs: websocketChannel(elevenLabsSocket),
      agent,
      // Blank keeps whatever voice the ElevenLabs agent itself is configured with.
      ...(agent.voice.trim() ? { voiceId: agent.voice.trim() } : {}),
      businessId,
      callSid,
      callId: call?.id ?? null,
      callerNumber: call?.fromNumber ?? null,
      logger: app.log,
      endCall,
      ...(dependencies.metrics ? { metrics: dependencies.metrics } : {}),
      onTranscript: persistTurn,
      onIdentifiers: ({ streamSid, sessionId }) => persistIdentifiers(streamSid, sessionId, provider),
    });

    guardDuration(() => bridge.close('max-duration'), elevenLabsSocket);
    track((reason) => bridge.close(reason), 'elevenlabs');
    bridge.start();
    return;
  }

  const openaiCredentials = providers.openai;
  if (!openaiCredentials) {
    app.log.error({ businessId, callSid, provider }, 'No platform credentials for the selected provider');
    await fallbackToGreeting(app, dependencies, businessId, callSid, 'provider-unavailable');
    socket.close(1011, 'provider unavailable');
    return;
  }
  const openaiSocket = await connectOpenAiRealtime({
    apiKey: openaiCredentials.apiKey,
    url: openaiCredentials.realtimeUrl,
    model: agent.realtimeModel,
  });
  app.log.info({ businessId, callSid, model: agent.realtimeModel }, 'OpenAI realtime connection opened');

  const bridge = new MediaStreamBridge({
    twilio: twilioChannel,
    openai: websocketChannel(openaiSocket),
    agent,
    businessId,
    callSid,
    callId: call?.id ?? null,
    callerNumber: call?.fromNumber ?? null,
    transcriptionModel: openaiCredentials.transcribeModel,
    logger: app.log,
    endCall,
    ...(dependencies.metrics ? { metrics: dependencies.metrics } : {}),
    onTranscript: persistTurn,
    onIdentifiers: ({ streamSid, sessionId }) => persistIdentifiers(streamSid, sessionId, 'openai'),
  });

  guardDuration(() => bridge.close('max-duration'), openaiSocket);
  track((reason) => bridge.close(reason), 'openai');
  bridge.start();
}

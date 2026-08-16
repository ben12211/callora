import type { FastifyInstance, FastifyReply } from 'fastify';
import twilio from 'twilio';
import { normalizeE164 } from '../dev/caller-allowlist.js';
import { registerApiRoutes } from './api-routes.js';
import { registerDashboardRoutes } from './dashboard/routes.js';
import type { ControlPlaneDependencies } from './dependencies.js';
import { MEDIA_STREAM_PATH, registerMediaStreamRoute } from './media-stream.js';
import { createStreamToken } from './stream-token.js';
import { callStatusSchema, e164Schema, incomingCallSchema } from './schemas.js';
import { twilioSignatureGuard } from './twilio-signature.js';

function validationError(reply: FastifyReply, issues: unknown): void {
  void reply.code(400).send({ error: 'Invalid request', issues });
}

export async function registerRoutes(
  app: FastifyInstance,
  dependencies: ControlPlaneDependencies,
): Promise<void> {
  const { config, store } = dependencies;
  const allowlist = dependencies.callerAllowlist ?? { enabled: false, allows: () => true };

  registerMediaStreamRoute(app, dependencies);
  await registerApiRoutes(app, dependencies);
  await registerDashboardRoutes(app, dependencies);

  app.get('/health', async (_request, reply) => {
    try {
      await store.ping();
      return { status: 'ok' };
    } catch (error) {
      app.log.error({ error }, 'Database health check failed');
      return reply.code(503).send({ status: 'unhealthy' });
    }
  });

  app.post(
    '/webhooks/twilio/voice',
    { preHandler: twilioSignatureGuard(config) },
    async (request, reply) => {
      const parsed = incomingCallSchema.safeParse(request.body);
      if (!parsed.success) {
        validationError(reply, parsed.error.issues);
        return;
      }

      // Development-only gate. This is the one place `From` decides anything; it never
      // takes part in tenant selection. A rejected call returns before any business
      // lookup, so no stream token is issued and no Realtime session is ever opened.
      if (allowlist.enabled && !allowlist.allows(parsed.data.From)) {
        app.log.warn(
          {
            callSid: parsed.data.CallSid,
            from: parsed.data.From,
            normalizedFrom: normalizeE164(parsed.data.From),
            to: parsed.data.To,
            matched: false,
          },
          'Rejected a caller that is not on the development allowlist',
        );
        const rejected = new twilio.twiml.VoiceResponse();
        rejected.say('This number is not available for testing right now.');
        rejected.hangup();
        return reply.type('text/xml; charset=utf-8').send(rejected.toString());
      }

      // The Twilio `To` number is the only tenant selector; `From` only identifies the caller.
      const business = await store.getBusinessByPhoneNumber(parsed.data.To);
      const response = new twilio.twiml.VoiceResponse();

      if (!business) {
        app.log.warn({ callSid: parsed.data.CallSid }, 'Incoming call for an unconfigured number');
        response.say('This phone number is not configured.');
        response.hangup();
        return reply.type('text/xml; charset=utf-8').send(response.toString());
      }

      const fromNumber = e164Schema.safeParse(parsed.data.From);
      await store.upsertCall({
        businessId: business.id,
        twilioCallSid: parsed.data.CallSid,
        fromNumber: fromNumber.success ? fromNumber.data : null,
        toNumber: parsed.data.To,
        status: parsed.data.CallStatus,
        direction: parsed.data.Direction ?? null,
      });

      const agent = await store.getAgentConfig(business.id);
      // The business picks its own provider; the platform still has to hold credentials
      // for it. Without them the call falls back to the static greeting instead of
      // opening a stream that could never connect.
      const providerReady = agent ? config.providers[agent.voiceProvider] !== null : false;
      app.log.info(
        {
          businessId: business.id,
          callSid: parsed.data.CallSid,
          agentEnabled: Boolean(agent?.enabled),
          voiceProvider: agent?.voiceProvider ?? null,
          providerReady,
        },
        'Business resolved for incoming call',
      );

      if (agent?.enabled && !providerReady) {
        app.log.error(
          { businessId: business.id, callSid: parsed.data.CallSid, voiceProvider: agent.voiceProvider },
          'The provider this business selected has no platform credentials; answering with the static greeting',
        );
      }

      if (agent?.enabled && providerReady) {
        // <Connect><Stream> is required for bidirectional audio; <Start><Stream> is one-way.
        const token = createStreamToken(config.twilioAuthToken, {
          callSid: parsed.data.CallSid,
          businessId: business.id,
        });
        const streamUrl = `${config.publicBaseUrl.replace(/^http/, 'ws')}${MEDIA_STREAM_PATH}`;
        const stream = response.connect().stream({ url: streamUrl });
        // Twilio deliberately drops query strings from <Stream> URLs. Custom
        // parameters are delivered in the WebSocket start event instead.
        stream.parameter({ name: 'token', value: token });
      } else {
        // No realtime agent configured: fall back to the static tenant greeting.
        response.say(business.greeting);
      }

      return reply.type('text/xml; charset=utf-8').send(response.toString());
    },
  );

  app.post(
    '/webhooks/twilio/call-status',
    { preHandler: twilioSignatureGuard(config) },
    async (request, reply) => {
      const parsed = callStatusSchema.safeParse(request.body);
      if (!parsed.success) {
        validationError(reply, parsed.error.issues);
        return;
      }

      const business = await store.getBusinessByPhoneNumber(parsed.data.To, false);
      if (business) {
        await store.updateCallStatus({
          businessId: business.id,
          twilioCallSid: parsed.data.CallSid,
          toNumber: parsed.data.To,
          status: parsed.data.CallStatus,
          durationSeconds: parsed.data.CallDuration ?? null,
        });
      }
      return reply.code(204).send();
    },
  );
}

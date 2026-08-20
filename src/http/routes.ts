import type { FastifyInstance, FastifyReply } from 'fastify';
import twilio from 'twilio';
import { normalizeE164 } from '../dev/caller-allowlist.js';
import { registerApiRoutes } from './api-routes.js';
import { requireApiAuth, requireBusinessScope } from './auth-guard.js';
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
  const { config, store, platform } = dependencies;

  registerMediaStreamRoute(app, dependencies);
  await registerApiRoutes(app, dependencies);
  await registerDashboardRoutes(app, dependencies);

  app.get('/health', async (_request, reply) => {
    const calls = app.callRegistry.snapshot();
    // A draining instance is deliberately unhealthy: the load balancer has to stop
    // sending it calls while the ones it already has finish.
    if (calls.draining) {
      return reply.code(503).send({ status: 'draining', activeCalls: calls.active });
    }

    try {
      await store.ping();
    } catch (error) {
      app.log.error({ error }, 'Database health check failed');
      return reply.code(503).send({ status: 'unhealthy', reason: 'database' });
    }

    // Reported, never fatal: a deployment with no provider credentials still answers
    // calls with the static greeting, and that is a valid state to be in.
    const providers = platform.providers();
    const ready = Object.entries(providers)
      .filter(([, credentials]) => credentials !== null)
      .map(([name]) => name);

    return {
      status: 'ok',
      activeCalls: calls.active,
      providersReady: ready,
    };
  });

  // Behind the same credential as the rest of the management surface: the label set
  // names businesses' providers and call volumes, which is not public information.
  app.get(
    '/metrics',
    // Registered outside the /api scope, so it carries both guards explicitly.
    { preHandler: [requireApiAuth(dependencies.auth), requireBusinessScope()] },
    async (_request, reply) => {
      return reply
        .type('text/plain; version=0.0.4; charset=utf-8')
        .send(app.metrics.render());
    },
  );

  app.post(
    '/webhooks/twilio/voice',
    { preHandler: twilioSignatureGuard(config) },
    async (request, reply) => {
      const parsed = incomingCallSchema.safeParse(request.body);
      if (!parsed.success) {
        validationError(reply, parsed.error.issues);
        return;
      }

      // Settings may have been saved on another instance, or straight into the
      // database, so the snapshot is re-checked before it decides anything about a call.
      await platform.refreshIfStale();

      // Testing gate, set in the dashboard or the environment. This is the one place
      // `From` decides anything; it never takes part in tenant selection. A rejected call
      // returns before any business lookup, so no stream token is issued and no realtime
      // session is ever opened.
      const allowlist = platform.allowlist();
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
      const providerReady = agent ? platform.providers()[agent.voiceProvider] !== null : false;
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
        // Minted with the newest secret; the media endpoint still accepts the previous
        // one, so introducing or rotating STREAM_TOKEN_SECRET drops no in-flight call.
        const token = createStreamToken(config.streamTokenSecrets[0] ?? config.twilioAuthToken, {
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

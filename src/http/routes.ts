import type { FastifyInstance, FastifyReply } from 'fastify';
import twilio from 'twilio';
import type { AppConfig } from '../config.js';
import type { DataStore } from '../db/store.js';
import type { CallerAllowlist } from '../dev/caller-allowlist.js';
import type { CallTerminator } from '../telephony/call-terminator.js';
import { MEDIA_STREAM_PATH, registerMediaStreamRoute } from './media-stream.js';
import { createStreamToken } from './stream-token.js';
import {
  callStatusSchema,
  callsQuerySchema,
  createBusinessSchema,
  e164Schema,
  idParamsSchema,
  incomingCallSchema,
  updateBusinessSchema,
} from './schemas.js';
import { twilioSignatureGuard } from './twilio-signature.js';

interface RouteDependencies {
  config: AppConfig;
  store: DataStore;
  /** Overridable so tests never reach the Twilio REST API. */
  callTerminator?: CallTerminator;
  /** Development-only caller gate; absent means every caller is allowed. */
  callerAllowlist?: CallerAllowlist;
}

function validationError(reply: FastifyReply, issues: unknown): void {
  void reply.code(400).send({ error: 'Invalid request', issues });
}

export async function registerRoutes(app: FastifyInstance, dependencies: RouteDependencies): Promise<void> {
  const { config, store } = dependencies;
  const allowlist = dependencies.callerAllowlist ?? { enabled: false, allows: () => true };

  registerMediaStreamRoute(app, dependencies);

  app.get('/health', async (_request, reply) => {
    try {
      await store.ping();
      return { status: 'ok' };
    } catch (error) {
      app.log.error({ error }, 'Database health check failed');
      return reply.code(503).send({ status: 'unhealthy' });
    }
  });

  app.get('/api/businesses', async () => ({ data: await store.listBusinesses() }));

  app.get('/api/businesses/:id', async (request, reply) => {
    const parsed = idParamsSchema.safeParse(request.params);
    if (!parsed.success) {
      validationError(reply, parsed.error.issues);
      return;
    }
    const business = await store.getBusinessById(parsed.data.id);
    if (!business) {
      return reply.code(404).send({ error: 'Business not found' });
    }
    return { data: business };
  });

  app.post('/api/businesses', async (request, reply) => {
    const parsed = createBusinessSchema.safeParse(request.body);
    if (!parsed.success) {
      validationError(reply, parsed.error.issues);
      return;
    }
    const business = await store.createBusiness(parsed.data);
    return reply.code(201).send({ data: business });
  });

  app.patch('/api/businesses/:id', async (request, reply) => {
    const params = idParamsSchema.safeParse(request.params);
    const body = updateBusinessSchema.safeParse(request.body);
    if (!params.success || !body.success) {
      validationError(reply, [
        ...(params.success ? [] : params.error.issues),
        ...(body.success ? [] : body.error.issues),
      ]);
      return;
    }
    const business = await store.updateBusiness(params.data.id, body.data);
    if (!business) {
      return reply.code(404).send({ error: 'Business not found' });
    }
    return { data: business };
  });

  app.delete('/api/businesses/:id', async (request, reply) => {
    const parsed = idParamsSchema.safeParse(request.params);
    if (!parsed.success) {
      validationError(reply, parsed.error.issues);
      return;
    }
    const deleted = await store.deleteBusiness(parsed.data.id);
    if (deleted) {
      return reply.code(204).send();
    }
    const existing = await store.getBusinessById(parsed.data.id);
    if (existing) {
      return reply.code(409).send({ error: 'Business with call history cannot be deleted; deactivate it instead' });
    }
    return reply.code(404).send({ error: 'Business not found' });
  });

  app.get('/api/calls', async (request, reply) => {
    const parsed = callsQuerySchema.safeParse(request.query);
    if (!parsed.success) {
      validationError(reply, parsed.error.issues);
      return;
    }
    return { data: await store.listCalls(parsed.data) };
  });

  app.get('/api/calls/:id', async (request, reply) => {
    const parsed = idParamsSchema.safeParse(request.params);
    if (!parsed.success) {
      validationError(reply, parsed.error.issues);
      return;
    }
    const call = await store.getCallById(parsed.data.id);
    if (!call) {
      return reply.code(404).send({ error: 'Call not found' });
    }
    return { data: call };
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
          { callSid: parsed.data.CallSid, to: parsed.data.To },
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
      app.log.info(
        {
          businessId: business.id,
          callSid: parsed.data.CallSid,
          agentEnabled: Boolean(agent?.enabled),
        },
        'Business resolved for incoming call',
      );

      if (agent?.enabled) {
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

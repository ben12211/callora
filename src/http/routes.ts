import type { FastifyInstance, FastifyReply } from 'fastify';
import twilio from 'twilio';
import type { AppConfig } from '../config.js';
import type { DataStore } from '../db/store.js';
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
}

function validationError(reply: FastifyReply, issues: unknown): void {
  void reply.code(400).send({ error: 'Invalid request', issues });
}

export async function registerRoutes(app: FastifyInstance, dependencies: RouteDependencies): Promise<void> {
  const { config, store } = dependencies;

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

      const business = await store.getBusinessByPhoneNumber(parsed.data.To);
      const response = new twilio.twiml.VoiceResponse();

      if (!business) {
        response.say('This phone number is not configured.');
        response.hangup();
      } else {
        const fromNumber = e164Schema.safeParse(parsed.data.From);
        await store.upsertCall({
          businessId: business.id,
          twilioCallSid: parsed.data.CallSid,
          fromNumber: fromNumber.success ? fromNumber.data : null,
          toNumber: parsed.data.To,
          status: parsed.data.CallStatus,
          direction: parsed.data.Direction ?? null,
        });
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

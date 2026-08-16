import type { FastifyInstance, FastifyReply } from 'fastify';
import type { AgentConfig, UpsertAgentConfigInput } from '../domain/models.js';
import { DEFAULT_TEXT_LLM_MODEL } from '../realtime/cartesia-constants.js';
import { providerStatuses } from '../realtime/provider-catalog.js';
import { requireApiAuth } from './auth-guard.js';
import {
  AGENT_AUDIT_FIELDS,
  AUDIT_ACTIONS,
  BUSINESS_AUDIT_FIELDS,
  changedFields,
} from './audit.js';
import type { ControlPlaneDependencies } from './dependencies.js';
import {
  agentConfigSchema,
  auditQuerySchema,
  callsQuerySchema,
  createBusinessSchema,
  idParamsSchema,
  updateBusinessSchema,
} from './schemas.js';

function validationError(reply: FastifyReply, issues: unknown): void {
  void reply.code(400).send({ error: 'Invalid request', issues });
}

/** Defaults for a business that has not been configured yet, kept disabled until an
 * operator fills the agent in, so a new number keeps answering with the static greeting. */
export function defaultAgentConfig(
  dependencies: ControlPlaneDependencies,
  greeting: string,
): UpsertAgentConfigInput {
  const { config } = dependencies;
  const provider = config.voiceProvider;
  // Each provider reads `voice` and `realtimeModel` differently, so the starting values
  // have to come from that provider. Blank means "keep the provider's own configured
  // voice", which only OpenAI has no answer for.
  const voice = {
    openai: 'marin',
    elevenlabs: '',
    cartesia: config.providers.cartesia?.defaultVoiceId ?? '',
  }[provider];

  return {
    enabled: false,
    language: 'he-IL',
    greeting,
    instructions: 'Describe the business, its tone, and what it can help callers with.',
    voiceProvider: provider,
    voice,
    realtimeModel:
      provider === 'cartesia'
        ? (config.providers.cartesia?.textLlmModel ?? DEFAULT_TEXT_LLM_MODEL)
        : 'gpt-realtime-2.1',
  };
}

/**
 * Management API. Every route here is behind authentication: a dashboard session cookie,
 * or the platform `ADMIN_API_KEY` for machine callers. The Twilio webhooks are not part
 * of this surface — they authenticate with Twilio's own request signature.
 */
export async function registerApiRoutes(
  app: FastifyInstance,
  dependencies: ControlPlaneDependencies,
): Promise<void> {
  const { store, config, auth, audit } = dependencies;

  app.register(async (api) => {
    api.addHook('preHandler', requireApiAuth(auth));

    api.get('/api/businesses', async () => ({ data: await store.listBusinesses() }));

    api.get('/api/businesses/:id', async (request, reply) => {
      const parsed = idParamsSchema.safeParse(request.params);
      if (!parsed.success) {
        validationError(reply, parsed.error.issues);
        return;
      }
      const business = await store.getBusinessById(parsed.data.id);
      if (!business) {
        return reply.code(404).send({ error: 'Business not found' });
      }
      const agent = await store.getAgentConfig(business.id);
      return { data: { ...business, agent } };
    });

    api.post('/api/businesses', async (request, reply) => {
      const parsed = createBusinessSchema.safeParse(request.body);
      if (!parsed.success) {
        validationError(reply, parsed.error.issues);
        return;
      }
      const business = await store.createBusiness(parsed.data);
      // Every business owns an agent row from the start, so the dashboard always has one
      // shape to edit and the call path always has an explicit enabled flag to read.
      const agent = await store.upsertAgentConfig(
        business.id,
        defaultAgentConfig(dependencies, business.greeting),
      );
      await audit.record(request.actor, {
        action: AUDIT_ACTIONS.businessCreated,
        entityType: 'business',
        entityId: business.id,
        summary: `Created business ${business.name} (${business.phoneNumber})`,
        details: { name: business.name, phoneNumber: business.phoneNumber, active: business.active },
      });
      return reply.code(201).send({ data: { ...business, agent } });
    });

    api.patch('/api/businesses/:id', async (request, reply) => {
      const params = idParamsSchema.safeParse(request.params);
      const body = updateBusinessSchema.safeParse(request.body);
      if (!params.success || !body.success) {
        validationError(reply, [
          ...(params.success ? [] : params.error.issues),
          ...(body.success ? [] : body.error.issues),
        ]);
        return;
      }
      const before = await store.getBusinessById(params.data.id);
      const business = await store.updateBusiness(params.data.id, body.data);
      if (!business) {
        return reply.code(404).send({ error: 'Business not found' });
      }
      const changes = changedFields(before, business, BUSINESS_AUDIT_FIELDS);
      const activationChanged = 'active' in changes;
      await audit.record(request.actor, {
        action: activationChanged
          ? business.active
            ? AUDIT_ACTIONS.businessEnabled
            : AUDIT_ACTIONS.businessDisabled
          : AUDIT_ACTIONS.businessUpdated,
        entityType: 'business',
        entityId: business.id,
        summary: activationChanged
          ? `${business.active ? 'Enabled' : 'Disabled'} business ${business.name}`
          : `Updated business ${business.name}`,
        details: { changes },
      });
      return { data: business };
    });

    api.delete('/api/businesses/:id', async (request, reply) => {
      const parsed = idParamsSchema.safeParse(request.params);
      if (!parsed.success) {
        validationError(reply, parsed.error.issues);
        return;
      }
      const deleted = await store.deleteBusiness(parsed.data.id);
      if (deleted) {
        await audit.record(request.actor, {
          action: AUDIT_ACTIONS.businessDeleted,
          entityType: 'business',
          entityId: deleted.id,
          summary: `Deleted business ${deleted.name} (${deleted.phoneNumber})`,
        });
        return reply.code(204).send();
      }
      const existing = await store.getBusinessById(parsed.data.id);
      if (existing) {
        return reply
          .code(409)
          .send({ error: 'Business with call history cannot be deleted; deactivate it instead' });
      }
      return reply.code(404).send({ error: 'Business not found' });
    });

    api.get('/api/businesses/:id/agent', async (request, reply) => {
      const parsed = idParamsSchema.safeParse(request.params);
      if (!parsed.success) {
        validationError(reply, parsed.error.issues);
        return;
      }
      const business = await store.getBusinessById(parsed.data.id);
      if (!business) {
        return reply.code(404).send({ error: 'Business not found' });
      }
      return { data: await store.getAgentConfig(business.id) };
    });

    api.put('/api/businesses/:id/agent', async (request, reply) => {
      const params = idParamsSchema.safeParse(request.params);
      const body = agentConfigSchema.safeParse(request.body);
      if (!params.success || !body.success) {
        validationError(reply, [
          ...(params.success ? [] : params.error.issues),
          ...(body.success ? [] : body.error.issues),
        ]);
        return;
      }
      const business = await store.getBusinessById(params.data.id);
      if (!business) {
        return reply.code(404).send({ error: 'Business not found' });
      }
      // Callora is the source of truth for configuration, but it cannot execute a call on
      // a provider it holds no credentials for. Refusing here surfaces the mistake in the
      // dashboard rather than as a dropped call.
      if (body.data.enabled && !config.providers[body.data.voiceProvider]) {
        return reply.code(422).send({
          error: `The ${body.data.voiceProvider} provider is not configured on this platform`,
        });
      }

      const before = await store.getAgentConfig(business.id);
      const agent = await store.upsertAgentConfig(business.id, body.data);
      await audit.record(request.actor, {
        action: AUDIT_ACTIONS.agentUpdated,
        entityType: 'agent',
        entityId: business.id,
        summary: `Updated the agent for ${business.name}`,
        details: { changes: changedFields<AgentConfig>(before, agent, AGENT_AUDIT_FIELDS) },
      });
      return { data: agent };
    });

    api.get('/api/providers', async () => ({
      data: providerStatuses(config.providers, config.voiceProvider),
    }));

    api.get('/api/calls', async (request, reply) => {
      const parsed = callsQuerySchema.safeParse(request.query);
      if (!parsed.success) {
        validationError(reply, parsed.error.issues);
        return;
      }
      return { data: await store.listCalls(parsed.data) };
    });

    api.get('/api/calls/:id', async (request, reply) => {
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

    api.get('/api/audit', async (request, reply) => {
      const parsed = auditQuerySchema.safeParse(request.query);
      if (!parsed.success) {
        validationError(reply, parsed.error.issues);
        return;
      }
      return { data: await store.listAuditEvents(parsed.data) };
    });

    api.get('/api/me', async (request) => ({
      data: {
        kind: request.actor?.kind,
        id: request.actor?.id,
        label: request.actor?.label,
        name: request.actor?.user?.name ?? null,
      },
    }));
  });
}

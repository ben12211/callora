import type { FastifyInstance, FastifyReply } from 'fastify';
import type { AgentConfig, UpsertAgentConfigInput } from '../domain/models.js';
import { DEFAULT_TEXT_LLM_MODEL } from '../realtime/cartesia-constants.js';
import { providerStatuses } from '../realtime/provider-catalog.js';
import { syncAgentToProvider } from './agent-sync.js';
import { canAccessBusiness, isPlatformActor } from '../auth/sessions.js';
import { requireApiAuth, requireBusinessScope } from './auth-guard.js';
import { API_RATE_LIMIT, RateLimiter } from './rate-limit.js';
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
  const { platform } = dependencies;
  const provider = platform.defaultProvider();
  const cartesia = platform.providers().cartesia;
  // Each provider reads `voice` and `realtimeModel` differently, so the starting values
  // have to come from that provider. Blank means "keep the provider's own configured
  // voice", which only OpenAI has no answer for.
  const voice = {
    openai: 'marin',
    elevenlabs: '',
    cartesia: cartesia?.defaultVoiceId ?? '',
  }[provider];

  return {
    enabled: false,
    language: 'he-IL',
    greeting,
    instructions: 'Describe the business, its tone, and what it can help callers with.',
    voiceProvider: provider,
    elevenLabsAgentId: '',
    voice,
    realtimeModel:
      provider === 'cartesia' ? (cartesia?.textLlmModel ?? DEFAULT_TEXT_LLM_MODEL) : 'gpt-realtime-2.1',
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
  const { store, platform, auth, audit } = dependencies;

  // Bounds a runaway script or a credential-stuffing loop. Ahead of authentication, so
  // an unauthenticated flood is rejected before it costs a session lookup.
  const apiLimiter = new RateLimiter(API_RATE_LIMIT);

  app.register(async (api) => {
    api.addHook('preHandler', async (request, reply) => {
      const decision = apiLimiter.check(request.ip);
      if (!decision.allowed) {
        app.log.warn({ ip: request.ip, url: request.url }, 'Rate limited a management API request');
        await reply
          .code(429)
          .header('retry-after', String(decision.retryAfterSeconds))
          .send({ error: 'Too many requests' });
      }
    });
    api.addHook('preHandler', requireApiAuth(auth));
    // Authorization, once, rather than a check inside each handler. Platform actors pass
    // through untouched, so a deployment with only platform accounts is unchanged.
    api.addHook('preHandler', requireBusinessScope());

    api.get('/api/businesses', async (request) => {
      const businesses = await store.listBusinesses();
      // A business administrator sees exactly their own tenant, never the platform's
      // customer list.
      return { data: businesses.filter((business) => canAccessBusiness(request.actor!, business.id)) };
    });

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
      if (body.data.enabled && !platform.providers()[body.data.voiceProvider]) {
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

      // Same contract as the dashboard: stored here, then pushed to the provider that
      // keeps its own copy. Reported alongside the saved row rather than as a failure.
      const sync = await syncAgentToProvider({
        agent,
        providers: platform.providers(),
        ...(dependencies.fetchImpl ? { fetchImpl: dependencies.fetchImpl } : {}),
      });
      if (!sync.skipped) {
        await audit.record(request.actor, {
          action: AUDIT_ACTIONS.agentSynced,
          entityType: 'agent',
          entityId: business.id,
          summary: sync.ok
            ? `Pushed the agent for ${business.name} to ElevenLabs`
            : `Failed to push the agent for ${business.name} to ElevenLabs`,
          details: { ok: sync.ok, message: sync.message },
        });
      }
      return { data: agent, sync: { ok: sync.ok, message: sync.message } };
    });

    api.get('/api/providers', async () => {
      await platform.refreshIfStale();
      return {
        data: providerStatuses(platform.providers(), platform.defaultProvider(), platform.environment()),
      };
    });

    api.get('/api/calls', async (request, reply) => {
      const parsed = callsQuerySchema.safeParse(request.query);
      if (!parsed.success) {
        validationError(reply, parsed.error.issues);
        return;
      }
      // A scoped administrator can only ever ask about their own business, whatever the
      // query string says.
      const actor = request.actor!;
      const scoped = isPlatformActor(actor)
        ? parsed.data
        : { ...parsed.data, businessId: actor.businessId ?? '' };
      return { data: await store.listCalls(scoped) };
    });

    api.get('/api/calls/:id', async (request, reply) => {
      const parsed = idParamsSchema.safeParse(request.params);
      if (!parsed.success) {
        validationError(reply, parsed.error.issues);
        return;
      }
      const call = await store.getCallById(parsed.data.id);
      // A call belonging to another business is reported as absent, not as forbidden.
      if (!call || !canAccessBusiness(request.actor!, call.businessId)) {
        return reply.code(404).send({ error: 'Call not found' });
      }
      return { data: call };
    });

    // The conversation itself. Previously it existed only as log lines, so there was no
    // way to review a call without access to the log aggregator.
    api.get('/api/calls/:id/transcript', async (request, reply) => {
      const parsed = idParamsSchema.safeParse(request.params);
      if (!parsed.success) {
        validationError(reply, parsed.error.issues);
        return;
      }
      const call = await store.getCallById(parsed.data.id);
      if (!call || !canAccessBusiness(request.actor!, call.businessId)) {
        return reply.code(404).send({ error: 'Call not found' });
      }
      return { data: await store.listTranscript(call.id) };
    });

    api.get('/api/audit', async (request, reply) => {
      const parsed = auditQuerySchema.safeParse(request.query);
      if (!parsed.success) {
        validationError(reply, parsed.error.issues);
        return;
      }
      const actor = request.actor!;
      const events = await store.listAuditEvents(parsed.data);
      if (isPlatformActor(actor)) {
        return { data: events };
      }
      // The audit history spans every tenant, so a scoped administrator sees only the
      // entries about their own business.
      return {
        data: events.filter(
          (event) => event.entityType === 'business' && event.entityId === actor.businessId,
        ),
      };
    });

    api.get('/api/me', async (request) => ({
      data: {
        kind: request.actor?.kind,
        id: request.actor?.id,
        label: request.actor?.label,
        name: request.actor?.user?.name ?? null,
        role: request.actor?.role,
        businessId: request.actor?.businessId ?? null,
      },
    }));
  });
}

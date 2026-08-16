import type { FastifyInstance, FastifyReply, FastifyRequest } from 'fastify';
import { hashPassword } from '../../auth/passwords.js';
import { CSRF_FIELD_NAME, SESSION_COOKIE_NAME, csrfTokenMatches, type AuthenticatedActor } from '../../auth/sessions.js';
import type { AgentConfig, Business } from '../../domain/models.js';
import { providerStatuses } from '../../realtime/provider-catalog.js';
import { defaultAgentConfig } from '../api-routes.js';
import { AGENT_AUDIT_FIELDS, AUDIT_ACTIONS, BUSINESS_AUDIT_FIELDS, changedFields } from '../audit.js';
import { clearCookie, readCookie, setCookie } from '../cookies.js';
import type { ControlPlaneDependencies } from '../dependencies.js';
import {
  agentConfigSchema,
  changePasswordSchema,
  createBusinessSchema,
  loginSchema,
  updateBusinessSchema,
} from '../schemas.js';
import { renderPage } from './layout.js';
import {
  auditPage,
  businessDetailPage,
  businessListPage,
  callDetailPage,
  callListPage,
  homePage,
  loginPage,
  newBusinessPage,
  notFoundPage,
  providerPage,
  settingsPage,
} from './pages.js';

const CALL_PAGE_SIZE = 25;
const AUDIT_PAGE_SIZE = 50;

function firstIssueMessage(issues: { path: PropertyKey[]; message: string }[]): string {
  const issue = issues[0];
  if (!issue) {
    return 'Invalid input';
  }
  const field = issue.path.map(String).join('.');
  return field ? `${field}: ${issue.message}` : issue.message;
}

/** Checkboxes are absent from a form post when unticked, which means false. */
function checkbox(body: unknown, field: string): boolean {
  return typeof body === 'object' && body !== null && (body as Record<string, unknown>)[field] !== undefined;
}

function readQueryString(request: FastifyRequest, key: string): string | undefined {
  const query = request.query as Record<string, unknown> | undefined;
  const value = query?.[key];
  return typeof value === 'string' && value.trim() !== '' ? value.trim() : undefined;
}

function readQueryNumber(request: FastifyRequest, key: string, fallback: number): number {
  const raw = readQueryString(request, key);
  const parsed = raw === undefined ? Number.NaN : Number(raw);
  return Number.isInteger(parsed) && parsed >= 0 ? parsed : fallback;
}

export async function registerDashboardRoutes(
  app: FastifyInstance,
  dependencies: ControlPlaneDependencies,
): Promise<void> {
  const { config, store, auth, audit } = dependencies;
  // A dev stack served over plain HTTP would drop a Secure cookie, so it follows the
  // scheme the deployment actually advertises rather than a hard-coded value.
  const secureCookies = config.publicBaseUrl.startsWith('https://');
  const cookieOptions = { secure: secureCookies, maxAgeSeconds: config.auth.sessionTtlHours * 3600 };

  const html = (reply: FastifyReply, body: string, status = 200): FastifyReply =>
    reply.code(status).type('text/html; charset=utf-8').send(body);

  const page = (
    reply: FastifyReply,
    request: FastifyRequest,
    actor: AuthenticatedActor,
    title: string,
    body: string,
    status = 200,
  ): FastifyReply =>
    html(
      reply,
      renderPage({
        title,
        currentPath: request.url.split('?')[0] ?? request.url,
        userLabel: actor.user?.name ?? actor.label,
        csrfToken: actor.session?.csrfToken ?? '',
        body,
      }),
      status,
    );

  /**
   * Dashboard pages need a real interactive session: an API key is a machine credential
   * and carries no CSRF token, so it cannot drive these forms.
   */
  const requireSession = async (
    request: FastifyRequest,
    reply: FastifyReply,
  ): Promise<AuthenticatedActor | null> => {
    const actor = await auth.resolveSession(readCookie(request, SESSION_COOKIE_NAME));
    if (!actor) {
      void reply.redirect('/dashboard/login', 303);
      return null;
    }
    return actor;
  };

  /** Every state-changing dashboard post carries the session's own CSRF token. */
  const checkCsrf = (actor: AuthenticatedActor, request: FastifyRequest, reply: FastifyReply): boolean => {
    const body = request.body as Record<string, unknown> | undefined;
    const expected = actor.session?.csrfToken ?? '';
    if (!expected || !csrfTokenMatches(expected, body?.[CSRF_FIELD_NAME])) {
      void html(reply, renderPage({ title: 'Invalid request', currentPath: request.url, body: notFoundPage('This form has expired. Reload the page and try again.') }), 400);
      return false;
    }
    return true;
  };

  const agentsByBusiness = async (businesses: Business[]): Promise<Map<string, AgentConfig>> => {
    const entries = await Promise.all(
      businesses.map(async (business) => [business.id, await store.getAgentConfig(business.id)] as const),
    );
    return new Map(
      entries.filter((entry): entry is [string, AgentConfig] => entry[1] !== null),
    );
  };

  app.get('/', async (_request, reply) => reply.redirect('/dashboard', 302));

  app.get('/dashboard/login', async (request, reply) => {
    const actor = await auth.resolveSession(readCookie(request, SESSION_COOKIE_NAME));
    if (actor) {
      return reply.redirect('/dashboard', 303);
    }
    return html(reply, renderPage({ title: 'Sign in', currentPath: '/dashboard/login', body: loginPage({}) }));
  });

  app.post('/dashboard/login', async (request, reply) => {
    const parsed = loginSchema.safeParse(request.body);
    if (!parsed.success) {
      return html(
        reply,
        renderPage({
          title: 'Sign in',
          currentPath: '/dashboard/login',
          body: loginPage({ error: 'Enter your email address and password.' }),
        }),
        400,
      );
    }

    const user = await auth.authenticate(parsed.data.email, parsed.data.password);
    if (!user) {
      // One message for every failure mode, so the form cannot be used to discover
      // which addresses have accounts.
      app.log.warn({ ip: request.ip, email: parsed.data.email }, 'Rejected a dashboard sign-in');
      return html(
        reply,
        renderPage({
          title: 'Sign in',
          currentPath: '/dashboard/login',
          body: loginPage({ error: 'Those credentials are not valid.', email: parsed.data.email }),
        }),
        401,
      );
    }

    const { token } = await auth.startSession(user);
    setCookie(reply, SESSION_COOKIE_NAME, token, cookieOptions);
    await audit.record(
      { id: user.id, label: user.email, kind: 'session' },
      {
        action: AUDIT_ACTIONS.adminLoggedIn,
        entityType: 'admin',
        entityId: user.id,
        summary: `${user.email} signed in`,
        details: { ip: request.ip },
      },
    );
    return reply.redirect('/dashboard', 303);
  });

  app.post('/dashboard/logout', async (request, reply) => {
    const token = readCookie(request, SESSION_COOKIE_NAME);
    await auth.endSession(token);
    clearCookie(reply, SESSION_COOKIE_NAME, { secure: secureCookies });
    return reply.redirect('/dashboard/login', 303);
  });

  app.get('/dashboard', async (request, reply) => {
    const actor = await requireSession(request, reply);
    if (!actor) return reply;
    const businesses = await store.listBusinesses();
    const [agents, callCount, recentCalls, recentAudit] = await Promise.all([
      agentsByBusiness(businesses),
      store.countCalls(),
      store.listCalls({ limit: 10, offset: 0 }),
      store.listAuditEvents({ limit: 10, offset: 0 }),
    ]);
    return page(
      reply,
      request,
      actor,
      'Dashboard',
      homePage({
        businesses,
        agents,
        callCount,
        recentCalls,
        recentAudit,
        providers: providerStatuses(config.providers, config.voiceProvider),
      }),
    );
  });

  app.get('/dashboard/businesses', async (request, reply) => {
    const actor = await requireSession(request, reply);
    if (!actor) return reply;
    const businesses = await store.listBusinesses();
    const agents = await agentsByBusiness(businesses);
    return page(reply, request, actor, 'Businesses', businessListPage(businesses, agents, readQueryString(request, 'notice')));
  });

  app.get('/dashboard/businesses/new', async (request, reply) => {
    const actor = await requireSession(request, reply);
    if (!actor) return reply;
    return page(
      reply,
      request,
      actor,
      'Create business',
      newBusinessPage({ csrfToken: actor.session?.csrfToken ?? '' }),
    );
  });

  app.post('/dashboard/businesses', async (request, reply) => {
    const actor = await requireSession(request, reply);
    if (!actor) return reply;
    if (!checkCsrf(actor, request, reply)) return reply;

    const body = request.body as Record<string, unknown>;
    const parsed = createBusinessSchema.safeParse({
      name: body['name'],
      phoneNumber: body['phoneNumber'],
      greeting: body['greeting'],
      active: checkbox(body, 'active'),
    });
    if (!parsed.success) {
      return page(
        reply,
        request,
        actor,
        'Create business',
        newBusinessPage({
          csrfToken: actor.session?.csrfToken ?? '',
          error: firstIssueMessage(parsed.error.issues),
          values: {
            name: String(body['name'] ?? ''),
            phoneNumber: String(body['phoneNumber'] ?? ''),
            greeting: String(body['greeting'] ?? ''),
          },
        }),
        400,
      );
    }

    const existing = await store.getBusinessByPhoneNumber(parsed.data.phoneNumber, false);
    if (existing) {
      return page(
        reply,
        request,
        actor,
        'Create business',
        newBusinessPage({
          csrfToken: actor.session?.csrfToken ?? '',
          error: `${parsed.data.phoneNumber} already belongs to ${existing.name}.`,
          values: parsed.data,
        }),
        409,
      );
    }

    const business = await store.createBusiness(parsed.data);
    await store.upsertAgentConfig(business.id, defaultAgentConfig(dependencies, business.greeting));
    await audit.record(actor, {
      action: AUDIT_ACTIONS.businessCreated,
      entityType: 'business',
      entityId: business.id,
      summary: `Created business ${business.name} (${business.phoneNumber})`,
      details: { name: business.name, phoneNumber: business.phoneNumber, active: business.active },
    });
    return reply.redirect(`/dashboard/businesses/${business.id}?notice=Business+created.+Configure+the+agent+below.`, 303);
  });

  app.get('/dashboard/businesses/:id', async (request, reply) => {
    const actor = await requireSession(request, reply);
    if (!actor) return reply;
    const { id } = request.params as { id: string };
    const business = await store.getBusinessById(id).catch(() => null);
    if (!business) {
      return page(reply, request, actor, 'Not found', notFoundPage('That business does not exist.'), 404);
    }
    const [agent, calls, auditEvents] = await Promise.all([
      store.getAgentConfig(business.id),
      store.listCalls({ businessId: business.id, limit: 10, offset: 0 }),
      store.listAuditEvents({ entityId: business.id, limit: 15, offset: 0 }),
    ]);
    return page(
      reply,
      request,
      actor,
      business.name,
      businessDetailPage({
        business,
        agent,
        calls,
        audit: auditEvents,
        providers: providerStatuses(config.providers, config.voiceProvider),
        csrfToken: actor.session?.csrfToken ?? '',
        ...(readQueryString(request, 'notice') ? { notice: readQueryString(request, 'notice')! } : {}),
        ...(readQueryString(request, 'error') ? { error: readQueryString(request, 'error')! } : {}),
      }),
    );
  });

  app.post('/dashboard/businesses/:id', async (request, reply) => {
    const actor = await requireSession(request, reply);
    if (!actor) return reply;
    if (!checkCsrf(actor, request, reply)) return reply;

    const { id } = request.params as { id: string };
    const body = request.body as Record<string, unknown>;
    const parsed = updateBusinessSchema.safeParse({
      name: body['name'],
      phoneNumber: body['phoneNumber'],
      greeting: body['greeting'],
      active: checkbox(body, 'active'),
    });
    if (!parsed.success) {
      return reply.redirect(
        `/dashboard/businesses/${id}?error=${encodeURIComponent(firstIssueMessage(parsed.error.issues))}`,
        303,
      );
    }

    const before = await store.getBusinessById(id).catch(() => null);
    if (!before) {
      return page(reply, request, actor, 'Not found', notFoundPage('That business does not exist.'), 404);
    }
    const business = await store.updateBusiness(id, parsed.data);
    if (!business) {
      return page(reply, request, actor, 'Not found', notFoundPage('That business does not exist.'), 404);
    }

    const changes = changedFields(before, business, BUSINESS_AUDIT_FIELDS);
    const activationChanged = 'active' in changes;
    await audit.record(actor, {
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
    return reply.redirect(`/dashboard/businesses/${business.id}?notice=Business+saved.`, 303);
  });

  app.post('/dashboard/businesses/:id/agent', async (request, reply) => {
    const actor = await requireSession(request, reply);
    if (!actor) return reply;
    if (!checkCsrf(actor, request, reply)) return reply;

    const { id } = request.params as { id: string };
    const body = request.body as Record<string, unknown>;
    const parsed = agentConfigSchema.safeParse({
      enabled: checkbox(body, 'enabled'),
      language: body['language'],
      greeting: body['greeting'],
      instructions: body['instructions'],
      voiceProvider: body['voiceProvider'],
      voice: body['voice'] ?? '',
      realtimeModel: body['realtimeModel'],
    });
    if (!parsed.success) {
      return reply.redirect(
        `/dashboard/businesses/${id}?error=${encodeURIComponent(firstIssueMessage(parsed.error.issues))}`,
        303,
      );
    }

    const business = await store.getBusinessById(id).catch(() => null);
    if (!business) {
      return page(reply, request, actor, 'Not found', notFoundPage('That business does not exist.'), 404);
    }
    if (parsed.data.enabled && !config.providers[parsed.data.voiceProvider]) {
      return reply.redirect(
        `/dashboard/businesses/${id}?error=${encodeURIComponent(
          `The ${parsed.data.voiceProvider} provider has no platform credentials, so the agent cannot be enabled on it.`,
        )}`,
        303,
      );
    }

    const before = await store.getAgentConfig(business.id);
    const agent = await store.upsertAgentConfig(business.id, parsed.data);
    await audit.record(actor, {
      action: AUDIT_ACTIONS.agentUpdated,
      entityType: 'agent',
      entityId: business.id,
      summary: `Updated the agent for ${business.name}`,
      details: { changes: changedFields<AgentConfig>(before, agent, AGENT_AUDIT_FIELDS) },
    });
    return reply.redirect(`/dashboard/businesses/${business.id}?notice=Agent+configuration+saved.`, 303);
  });

  app.get('/dashboard/calls', async (request, reply) => {
    const actor = await requireSession(request, reply);
    if (!actor) return reply;
    const businessId = readQueryString(request, 'businessId');
    const offset = readQueryNumber(request, 'offset', 0);
    const businesses = await store.listBusinesses();
    const known = businessId && businesses.some((business) => business.id === businessId) ? businessId : undefined;
    const calls = await store.listCalls({
      ...(known ? { businessId: known } : {}),
      limit: CALL_PAGE_SIZE,
      offset,
    });
    return page(
      reply,
      request,
      actor,
      'Calls',
      callListPage({
        calls,
        businesses,
        ...(known ? { businessId: known } : {}),
        limit: CALL_PAGE_SIZE,
        offset,
      }),
    );
  });

  app.get('/dashboard/calls/:id', async (request, reply) => {
    const actor = await requireSession(request, reply);
    if (!actor) return reply;
    const { id } = request.params as { id: string };
    const call = await store.getCallById(id).catch(() => null);
    if (!call) {
      return page(reply, request, actor, 'Not found', notFoundPage('That call does not exist.'), 404);
    }
    const business = await store.getBusinessById(call.businessId);
    return page(reply, request, actor, 'Call detail', callDetailPage(call, business));
  });

  app.get('/dashboard/providers', async (request, reply) => {
    const actor = await requireSession(request, reply);
    if (!actor) return reply;
    return page(
      reply,
      request,
      actor,
      'Providers',
      providerPage(providerStatuses(config.providers, config.voiceProvider), config.voiceProvider),
    );
  });

  app.get('/dashboard/audit', async (request, reply) => {
    const actor = await requireSession(request, reply);
    if (!actor) return reply;
    const offset = readQueryNumber(request, 'offset', 0);
    const events = await store.listAuditEvents({ limit: AUDIT_PAGE_SIZE, offset });
    return page(reply, request, actor, 'Audit history', auditPage(events, AUDIT_PAGE_SIZE, offset));
  });

  app.get('/dashboard/settings', async (request, reply) => {
    const actor = await requireSession(request, reply);
    if (!actor?.user) return reply;
    return page(
      reply,
      request,
      actor,
      'Settings',
      settingsPage({
        user: actor.user,
        admins: await store.listAdminUsers(),
        providers: providerStatuses(config.providers, config.voiceProvider),
        publicBaseUrl: config.publicBaseUrl,
        platformDefault: config.voiceProvider,
        sessionTtlHours: config.auth.sessionTtlHours,
        apiKeyConfigured: Boolean(config.auth.apiKey),
        csrfToken: actor.session?.csrfToken ?? '',
        ...(readQueryString(request, 'notice') ? { notice: readQueryString(request, 'notice')! } : {}),
        ...(readQueryString(request, 'error') ? { error: readQueryString(request, 'error')! } : {}),
      }),
    );
  });

  app.post('/dashboard/settings/password', async (request, reply) => {
    const actor = await requireSession(request, reply);
    if (!actor?.user) return reply;
    if (!checkCsrf(actor, request, reply)) return reply;

    const parsed = changePasswordSchema.safeParse(request.body);
    if (!parsed.success) {
      return reply.redirect(
        `/dashboard/settings?error=${encodeURIComponent(firstIssueMessage(parsed.error.issues))}`,
        303,
      );
    }
    const verified = await auth.authenticate(actor.user.email, parsed.data.currentPassword);
    if (!verified) {
      return reply.redirect(`/dashboard/settings?error=${encodeURIComponent('Your current password is not correct.')}`, 303);
    }

    await store.setAdminUserPassword(actor.user.id, await hashPassword(parsed.data.newPassword));
    // Every session for this account is invalidated, including this one, so a stolen
    // cookie cannot outlive the password it was issued under.
    await store.deleteAdminSessionsForUser(actor.user.id);
    await audit.record(actor, {
      action: AUDIT_ACTIONS.adminPasswordChanged,
      entityType: 'admin',
      entityId: actor.user.id,
      summary: `${actor.user.email} changed their password`,
    });
    clearCookie(reply, SESSION_COOKIE_NAME, { secure: secureCookies });
    return reply.redirect('/dashboard/login', 303);
  });
}

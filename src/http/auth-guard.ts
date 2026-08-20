import type { FastifyReply, FastifyRequest } from 'fastify';
import type { AuthenticatedActor, AuthService } from '../auth/sessions.js';
import { SESSION_COOKIE_NAME, canAccessBusiness, isPlatformActor } from '../auth/sessions.js';
import { readCookie } from './cookies.js';

declare module 'fastify' {
  interface FastifyRequest {
    /** Present only after `requireApiAuth` or `requireDashboardAuth` has run. */
    actor?: AuthenticatedActor;
  }
}

/**
 * Resolves the caller without deciding anything. Both the JSON API and the dashboard
 * accept a session cookie; the API additionally accepts the platform admin API key so
 * scripts and deploy checks do not need a browser.
 */
async function resolveActor(
  auth: AuthService,
  request: FastifyRequest,
  options: { allowApiKey: boolean },
): Promise<AuthenticatedActor | null> {
  if (options.allowApiKey) {
    const header = request.headers['x-api-key'];
    const presented = Array.isArray(header) ? header[0] : header;
    const byKey = auth.verifyApiKey(presented);
    if (byKey) {
      return byKey;
    }
  }
  return auth.resolveSession(readCookie(request, SESSION_COOKIE_NAME));
}

/** Management API guard: 401 with no body detail, so it cannot be probed for accounts. */
export function requireApiAuth(auth: AuthService) {
  return async function guard(request: FastifyRequest, reply: FastifyReply): Promise<void> {
    const actor = await resolveActor(auth, request, { allowApiKey: true });
    if (!actor) {
      void reply
        .code(401)
        .header('www-authenticate', 'Session realm="callora"')
        .send({ error: 'Authentication required' });
      return;
    }
    request.actor = actor;
  };
}

/**
 * Authorization for a business-scoped administrator.
 *
 * Kept as one hook rather than a check inside each handler, because authorization spread
 * across call sites is authorization that eventually gets forgotten at one of them. A
 * platform actor passes through untouched, so every existing deployment behaves exactly
 * as it did before roles existed.
 *
 * A tenant asking about a business that is not theirs gets 404, not 403: whether another
 * business exists on this platform is not theirs to learn.
 */
export function requireBusinessScope() {
  return async function guard(request: FastifyRequest, reply: FastifyReply): Promise<void> {
    const actor = request.actor;
    if (!actor || isPlatformActor(actor)) {
      return;
    }

    const method = request.method.toUpperCase();
    const path = request.url.split('?')[0] ?? '';

    // Platform-wide surfaces. Provider credentials and the tenant list itself belong to
    // whoever runs the platform, not to one of its tenants.
    if (path.startsWith('/api/providers') || path.startsWith('/metrics')) {
      await denied(reply);
      return;
    }
    // Creating a business, or deleting one, is not a tenant's decision about itself.
    if (path === '/api/businesses' && method !== 'GET') {
      await denied(reply);
      return;
    }
    if (method === 'DELETE' && path.startsWith('/api/businesses/')) {
      await denied(reply);
      return;
    }

    const businessId = businessIdFromPath(path);
    if (businessId && !canAccessBusiness(actor, businessId)) {
      await reply.code(404).send({ error: 'Not found' });
    }
  };
}

/** `/api/businesses/<id>` and anything nested under it. */
function businessIdFromPath(path: string): string | null {
  const match = /^\/api\/businesses\/([^/]+)/.exec(path);
  return match?.[1] ?? null;
}

async function denied(reply: FastifyReply): Promise<void> {
  await reply.code(403).send({ error: 'This account is scoped to a single business' });
}

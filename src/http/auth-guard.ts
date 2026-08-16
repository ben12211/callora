import type { FastifyReply, FastifyRequest } from 'fastify';
import type { AuthenticatedActor, AuthService } from '../auth/sessions.js';
import { SESSION_COOKIE_NAME } from '../auth/sessions.js';
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

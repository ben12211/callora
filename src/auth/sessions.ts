import { createHash, randomBytes, timingSafeEqual } from 'node:crypto';
import type { AdminAuthConfig } from '../config.js';
import type { DataStore } from '../db/store.js';
import type { AdminRole, AdminSession, AdminUser } from '../domain/models.js';
import { burnPasswordComparison, verifyPassword } from './passwords.js';

export const SESSION_COOKIE_NAME = 'callora_session';
export const CSRF_FIELD_NAME = '_csrf';

/** Only the digest is stored, so a leaked database row cannot be replayed as a login. */
export function hashSessionToken(token: string): string {
  return createHash('sha256').update(token).digest('hex');
}

export interface AuthenticatedActor {
  id: string | null;
  label: string;
  /** How the request proved who it is. API keys are machine callers and have no session. */
  kind: 'session' | 'api-key';
  /**
   * What this actor may reach. The `ADMIN_API_KEY` machine credential is a platform
   * credential, so it stays platform-scoped.
   */
  role: AdminRole;
  /** The single business a `business` actor is confined to; null for platform actors. */
  businessId: string | null;
  session?: AdminSession;
  user?: AdminUser;
}

/** True when the actor may reach every tenant and the platform's own settings. */
export function isPlatformActor(actor: AuthenticatedActor): boolean {
  return actor.role === 'platform';
}

/**
 * Whether this actor may touch one business.
 *
 * Deliberately a single function: authorization spread across call sites is authorization
 * that gets forgotten at one of them.
 */
export function canAccessBusiness(actor: AuthenticatedActor, businessId: string): boolean {
  return isPlatformActor(actor) || actor.businessId === businessId;
}

export class AuthService {
  public constructor(
    private readonly store: DataStore,
    private readonly config: AdminAuthConfig,
  ) {}

  /**
   * Verifies credentials. Returns null for an unknown email, a deactivated account, or a
   * wrong password alike, so the response cannot be used to enumerate administrators.
   */
  public async authenticate(email: string, password: string): Promise<AdminUser | null> {
    const user = await this.store.getAdminUserByEmail(email.trim().toLowerCase());
    if (!user || !user.active) {
      // Still spend the KDF time, so a missing account is not measurably faster.
      await burnPasswordComparison(password);
      return null;
    }
    return (await verifyPassword(password, user.passwordHash)) ? user : null;
  }

  /** Issues a session and returns the raw cookie value, which is never stored anywhere. */
  public async startSession(user: AdminUser): Promise<{ token: string; session: AdminSession }> {
    const token = randomBytes(32).toString('base64url');
    const expiresAt = new Date(Date.now() + this.config.sessionTtlHours * 60 * 60 * 1000);
    const session = await this.store.createAdminSession({
      adminUserId: user.id,
      tokenHash: hashSessionToken(token),
      csrfToken: randomBytes(24).toString('base64url'),
      expiresAt,
    });
    await this.store.recordAdminLogin(user.id);
    return { token, session };
  }

  public async resolveSession(token: string | undefined): Promise<AuthenticatedActor | null> {
    if (!token) {
      return null;
    }
    const found = await this.store.findAdminSession(hashSessionToken(token));
    if (!found) {
      return null;
    }
    return {
      id: found.user.id,
      label: found.user.email,
      kind: 'session',
      role: found.user.role,
      businessId: found.user.businessId,
      session: found.session,
      user: found.user,
    };
  }

  public async endSession(token: string | undefined): Promise<void> {
    if (token) {
      await this.store.deleteAdminSession(hashSessionToken(token));
    }
  }

  /** Machine access to the management API. Absent configuration means no key is accepted. */
  public verifyApiKey(presented: string | undefined): AuthenticatedActor | null {
    const expected = this.config.apiKey;
    if (!expected || !presented || presented.length !== expected.length) {
      return null;
    }
    const matches = timingSafeEqual(Buffer.from(presented, 'utf8'), Buffer.from(expected, 'utf8'));
    return matches
      ? { id: null, label: 'api-key', kind: 'api-key', role: 'platform', businessId: null }
      : null;
  }

  public sessionTtlHours(): number {
    return this.config.sessionTtlHours;
  }
}

/** Constant-time comparison for the per-session CSRF token carried by dashboard forms. */
export function csrfTokenMatches(expected: string, presented: unknown): boolean {
  if (typeof presented !== 'string' || presented.length !== expected.length) {
    return false;
  }
  return timingSafeEqual(Buffer.from(presented, 'utf8'), Buffer.from(expected, 'utf8'));
}

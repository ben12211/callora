import type { AdminAuthConfig } from '../config.js';
import type { DataStore } from '../db/store.js';
import { hashPassword, verifyPassword } from './passwords.js';

export interface BootstrapLogger {
  info(details: Record<string, unknown>, message: string): void;
  warn(details: Record<string, unknown>, message: string): void;
}

/**
 * Applies the bootstrap administrator from the environment.
 *
 * A fresh stack has no way in otherwise, and an operator who has lost access needs one
 * that does not involve hand-writing a scrypt hash. `ADMIN_EMAIL` and `ADMIN_PASSWORD`
 * together create the account, and reset its password if it already exists — so the
 * environment stays the source of truth for the first credential. Rotating a password in
 * the dashboard and then leaving the old value in the environment would silently undo
 * the change, so an unchanged password is detected and left alone.
 */
export async function ensureBootstrapAdmin(
  store: DataStore,
  auth: AdminAuthConfig,
  logger: BootstrapLogger,
): Promise<void> {
  const email = auth.bootstrapEmail;
  const password = auth.bootstrapPassword;

  if (!email || !password) {
    const existing = await store.listAdminUsers();
    if (existing.length === 0) {
      logger.warn(
        {},
        'No administrator exists and ADMIN_EMAIL/ADMIN_PASSWORD are unset; the dashboard cannot be signed into',
      );
    }
    return;
  }

  const existing = await store.getAdminUserByEmail(email);
  if (!existing) {
    const created = await store.createAdminUser({
      email,
      name: auth.bootstrapName,
      passwordHash: await hashPassword(password),
    });
    logger.info({ adminUserId: created.id, email: created.email }, 'Created the bootstrap administrator');
    return;
  }

  if (await verifyPassword(password, existing.passwordHash)) {
    return;
  }

  await store.setAdminUserPassword(existing.id, await hashPassword(password));
  // Any session issued under the previous password is no longer valid.
  await store.deleteAdminSessionsForUser(existing.id);
  logger.warn(
    { adminUserId: existing.id, email: existing.email },
    'Reset the bootstrap administrator password from the environment',
  );
}

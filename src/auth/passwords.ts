import { randomBytes, scrypt as scryptCallback, timingSafeEqual } from 'node:crypto';
import { promisify } from 'node:util';

const scrypt = promisify(scryptCallback) as (
  password: string,
  salt: Buffer,
  keylen: number,
) => Promise<Buffer>;

/**
 * Password storage for dashboard administrators.
 *
 * scrypt from the Node standard library keeps the dependency surface at zero while still
 * being a memory-hard KDF. The encoded form carries its own parameters, so raising the
 * cost later does not invalidate existing hashes.
 */
const KEY_LENGTH = 64;
const SALT_LENGTH = 16;
const SCHEME = 'scrypt';

export async function hashPassword(password: string): Promise<string> {
  const salt = randomBytes(SALT_LENGTH);
  const derived = await scrypt(password, salt, KEY_LENGTH);
  return `${SCHEME}$${salt.toString('base64url')}$${derived.toString('base64url')}`;
}

export async function verifyPassword(password: string, encoded: string): Promise<boolean> {
  const [scheme, salt, expected] = encoded.split('$');
  if (scheme !== SCHEME || !salt || !expected) {
    return false;
  }

  let expectedBytes: Buffer;
  try {
    expectedBytes = Buffer.from(expected, 'base64url');
  } catch {
    return false;
  }
  if (expectedBytes.length !== KEY_LENGTH) {
    return false;
  }

  const derived = await scrypt(password, Buffer.from(salt, 'base64url'), KEY_LENGTH);
  return timingSafeEqual(derived, expectedBytes);
}

/**
 * Burns the same derivation cost as a real verification. Login uses it when no account
 * matches, so an unknown email is not measurably faster than a wrong password.
 */
export async function burnPasswordComparison(password: string): Promise<void> {
  await scrypt(password, Buffer.alloc(SALT_LENGTH), KEY_LENGTH);
}

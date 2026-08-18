import { createCipheriv, createDecipheriv, randomBytes, scryptSync } from 'node:crypto';

/**
 * Authenticated encryption for the provider credentials the dashboard writes.
 *
 * A database that holds API keys is a different kind of asset from one that holds
 * business names, and a backup of it is the same asset again. Sealing the values means a
 * dump, a replica, or a stolen snapshot carries nothing usable on its own: the key lives
 * in `SECRETS_KEY` in the deployment environment and never in a row.
 *
 * GCM is what makes a tampered ciphertext fail loudly instead of decrypting to garbage,
 * which matters when the plaintext is about to be sent to a provider as a credential.
 */

const ALGORITHM = 'aes-256-gcm';
const VERSION = 'v1';
/** Fixed, non-secret salt: the entropy is in SECRETS_KEY, and rows must decrypt across restarts. */
const KEY_SALT = 'callora.platform-settings.v1';
const IV_BYTES = 12;

export class SecretBox {
  private readonly key: Buffer;

  /** Accepts a passphrase of any length; scrypt turns it into the 32-byte AES key. */
  public constructor(masterKey: string) {
    this.key = scryptSync(masterKey, KEY_SALT, 32);
  }

  public seal(plaintext: string): string {
    const iv = randomBytes(IV_BYTES);
    const cipher = createCipheriv(ALGORITHM, this.key, iv);
    const ciphertext = Buffer.concat([cipher.update(plaintext, 'utf8'), cipher.final()]);
    return [VERSION, iv, cipher.getAuthTag(), ciphertext]
      .map((part) => (typeof part === 'string' ? part : part.toString('base64url')))
      .join('.');
  }

  /** Throws when the value was written under a different key or has been altered. */
  public open(sealed: string): string {
    const [version, iv, tag, ciphertext] = sealed.split('.');
    if (version !== VERSION || !iv || !tag || !ciphertext) {
      throw new Error('The stored secret is not in a format this build understands');
    }
    const decipher = createDecipheriv(ALGORITHM, this.key, Buffer.from(iv, 'base64url'));
    decipher.setAuthTag(Buffer.from(tag, 'base64url'));
    return Buffer.concat([
      decipher.update(Buffer.from(ciphertext, 'base64url')),
      decipher.final(),
    ]).toString('utf8');
  }
}

/**
 * A box only when the deployment supplies a key. Without one the dashboard keeps managing
 * every non-secret setting and reports that credential editing needs `SECRETS_KEY`,
 * rather than writing API keys to the database in the clear.
 */
export function createSecretBox(masterKey?: string): SecretBox | null {
  return masterKey ? new SecretBox(masterKey) : null;
}

#!/usr/bin/env node
/**
 * Re-encrypts the stored provider credentials under a new `SECRETS_KEY`.
 *
 * `SECRETS_KEY` had no rotation path: changing it made every stored credential
 * undecryptable, and the only recovery was to re-enter each one by hand in the dashboard.
 * That is the kind of trap that makes a key never get rotated at all, including after it
 * leaks.
 *
 * Usage:
 *
 *   SECRETS_KEY_OLD=... SECRETS_KEY_NEW=... DATABASE_URL=... node scripts/rotate-secrets-key.mjs
 *
 * Add `--dry-run` to report what would change without writing. The rewrite runs in one
 * transaction, so the table is never left half-rotated; if it fails, nothing moved and
 * the old key still works.
 *
 * Afterwards, set `SECRETS_KEY` to the new value and restart. Until you do, the running
 * process cannot read the rotated rows and will fall back to the environment, which is
 * the same behaviour it already has for a value it cannot decrypt.
 */

import { createCipheriv, createDecipheriv, randomBytes, scryptSync } from 'node:crypto';
import pg from 'pg';

// Must match src/platform/secret-box.ts exactly.
const ALGORITHM = 'aes-256-gcm';
const VERSION = 'v1';
const KEY_SALT = 'callora.platform-settings.v1';
const IV_BYTES = 12;

function deriveKey(masterKey) {
  return scryptSync(masterKey, KEY_SALT, 32);
}

function open(key, sealed) {
  const [version, iv, tag, ciphertext] = sealed.split('.');
  if (version !== VERSION || !iv || !tag || !ciphertext) {
    throw new Error('not in a format this script understands');
  }
  const decipher = createDecipheriv(ALGORITHM, key, Buffer.from(iv, 'base64url'));
  decipher.setAuthTag(Buffer.from(tag, 'base64url'));
  return Buffer.concat([
    decipher.update(Buffer.from(ciphertext, 'base64url')),
    decipher.final(),
  ]).toString('utf8');
}

function seal(key, plaintext) {
  const iv = randomBytes(IV_BYTES);
  const cipher = createCipheriv(ALGORITHM, key, iv);
  const ciphertext = Buffer.concat([cipher.update(plaintext, 'utf8'), cipher.final()]);
  return [VERSION, iv.toString('base64url'), cipher.getAuthTag().toString('base64url'), ciphertext.toString('base64url')].join('.');
}

function fail(message) {
  process.stderr.write(`${message}\n`);
  process.exit(1);
}

const oldKey = process.env['SECRETS_KEY_OLD'];
const newKey = process.env['SECRETS_KEY_NEW'];
const databaseUrl = process.env['DATABASE_URL'];
const dryRun = process.argv.includes('--dry-run');

if (!oldKey || !newKey) {
  fail('Set SECRETS_KEY_OLD and SECRETS_KEY_NEW.');
}
if (!databaseUrl) {
  fail('Set DATABASE_URL.');
}
if (oldKey === newKey) {
  fail('SECRETS_KEY_OLD and SECRETS_KEY_NEW are the same; nothing to rotate.');
}
if (newKey.length < 16) {
  fail('SECRETS_KEY_NEW must be at least 16 characters, matching what the server accepts.');
}

const from = deriveKey(oldKey);
const to = deriveKey(newKey);

const client = new pg.Client({ connectionString: databaseUrl });
await client.connect();

let rotated = 0;
let skipped = 0;
const unreadable = [];

try {
  await client.query('BEGIN');
  // Locks the rows for the duration, so a dashboard save cannot interleave with the
  // rewrite and end up sealed under the key that is going away.
  const { rows } = await client.query(
    'SELECT key, value FROM platform_settings WHERE secret = true FOR UPDATE',
  );

  for (const row of rows) {
    let plaintext;
    try {
      plaintext = open(from, row.value);
    } catch {
      // Already under the new key, or under neither. Reported by name only — never value.
      try {
        open(to, row.value);
        skipped += 1;
      } catch {
        unreadable.push(row.key);
      }
      continue;
    }

    if (!dryRun) {
      await client.query('UPDATE platform_settings SET value = $2, updated_at = now() WHERE key = $1', [
        row.key,
        seal(to, plaintext),
      ]);
    }
    rotated += 1;
  }

  if (unreadable.length > 0) {
    throw new Error(
      `These settings decrypt under neither key and would be lost: ${unreadable.join(', ')}. ` +
        'Re-enter them in the dashboard first, or delete them, then run this again.',
    );
  }

  if (dryRun) {
    await client.query('ROLLBACK');
  } else {
    await client.query('COMMIT');
  }
} catch (error) {
  await client.query('ROLLBACK');
  process.stderr.write(`${error instanceof Error ? error.message : 'Rotation failed'}\n`);
  await client.end();
  process.exit(1);
} finally {
  await client.end().catch(() => {});
}

process.stdout.write(
  `${dryRun ? 'Would rotate' : 'Rotated'} ${rotated} credential(s); ${skipped} already under the new key.\n` +
    (dryRun ? 'Nothing was written.\n' : 'Set SECRETS_KEY to the new value and restart.\n'),
);

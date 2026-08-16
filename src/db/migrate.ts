import { readdir, readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import type pg from 'pg';
import { loadConfig } from '../config.js';
import { DEFAULT_TEXT_LLM_MODEL } from '../realtime/cartesia-constants.js';
import { DEFAULT_REALTIME_PROVIDER, REALTIME_PROVIDERS } from '../realtime/provider.js';
import { createPool } from './pool.js';

/**
 * Exposes the deployment's platform-wide voice settings to the migration SQL.
 *
 * Before per-business provider selection existed, every agent ran on `VOICE_PROVIDER`,
 * and its `voice` and `realtime_model` were written for that provider. A migration that
 * simply defaulted the new column to `openai` would silently move an ElevenLabs or
 * Cartesia deployment onto the wrong provider on upgrade, so the backfill needs to know
 * what the deployment was actually running.
 *
 * Values are validated here and passed through `set_config`, never interpolated.
 */
async function publishPlatformDefaults(client: pg.PoolClient): Promise<void> {
  const requested = (process.env['VOICE_PROVIDER'] ?? '').trim();
  const provider = (REALTIME_PROVIDERS as readonly string[]).includes(requested)
    ? requested
    : DEFAULT_REALTIME_PROVIDER;
  const textLlmModel = (process.env['TEXT_LLM_MODEL'] ?? '').trim() || DEFAULT_TEXT_LLM_MODEL;

  await client.query('SELECT set_config($1, $2, false)', ['callora.default_voice_provider', provider]);
  await client.query('SELECT set_config($1, $2, false)', ['callora.default_text_llm_model', textLlmModel]);
}

export async function runMigrations(pool: pg.Pool, directory = resolve(process.cwd(), 'migrations')): Promise<void> {
  const client = await pool.connect();
  try {
    await client.query("SELECT pg_advisory_lock(hashtext('callora_schema_migrations'))");
    await publishPlatformDefaults(client);
    await client.query(`
      CREATE TABLE IF NOT EXISTS schema_migrations (
        name text PRIMARY KEY,
        applied_at timestamptz NOT NULL DEFAULT now()
      )
    `);

    const files = (await readdir(directory)).filter((file) => file.endsWith('.sql')).sort();

    for (const file of files) {
      const alreadyApplied = await client.query('SELECT 1 FROM schema_migrations WHERE name = $1', [file]);
      if (alreadyApplied.rowCount) {
        continue;
      }

      const sql = await readFile(resolve(directory, file), 'utf8');
      await client.query('BEGIN');
      try {
        await client.query(sql);
        await client.query('INSERT INTO schema_migrations (name) VALUES ($1)', [file]);
        await client.query('COMMIT');
      } catch (error) {
        await client.query('ROLLBACK');
        throw error;
      }
    }
  } finally {
    await client.query("SELECT pg_advisory_unlock(hashtext('callora_schema_migrations'))").catch(() => undefined);
    client.release();
  }
}

async function main(): Promise<void> {
  const config = loadConfig();
  const pool = createPool(config.databaseUrl);
  try {
    await runMigrations(pool);
  } finally {
    await pool.end();
  }
}

if (process.argv[1]?.endsWith('migrate.js') || process.argv[1]?.endsWith('migrate.ts')) {
  await main();
}

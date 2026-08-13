import { readdir, readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import type pg from 'pg';
import { loadConfig } from '../config.js';
import { createPool } from './pool.js';

export async function runMigrations(pool: pg.Pool, directory = resolve(process.cwd(), 'migrations')): Promise<void> {
  await pool.query(`
    CREATE TABLE IF NOT EXISTS schema_migrations (
      name text PRIMARY KEY,
      applied_at timestamptz NOT NULL DEFAULT now()
    )
  `);

  const files = (await readdir(directory)).filter((file) => file.endsWith('.sql')).sort();

  for (const file of files) {
    const alreadyApplied = await pool.query('SELECT 1 FROM schema_migrations WHERE name = $1', [file]);
    if (alreadyApplied.rowCount) {
      continue;
    }

    const sql = await readFile(resolve(directory, file), 'utf8');
    const client = await pool.connect();
    try {
      await client.query('BEGIN');
      await client.query(sql);
      await client.query('INSERT INTO schema_migrations (name) VALUES ($1)', [file]);
      await client.query('COMMIT');
    } catch (error) {
      await client.query('ROLLBACK');
      throw error;
    } finally {
      client.release();
    }
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

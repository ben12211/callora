import type pg from 'pg';
import { loadConfig } from '../config.js';
import { createPool } from './pool.js';

const exampleBusinessId = '00000000-0000-4000-8000-000000000001';

export async function seedDatabase(pool: pg.Pool): Promise<void> {
  await pool.query(
    `INSERT INTO businesses (id, name, phone_number, greeting, active)
     VALUES ($1, $2, $3, $4, true)
     ON CONFLICT (id) DO NOTHING`,
    [
      exampleBusinessId,
      'Continue Demo Business',
      '+15551234567',
      'Thanks for calling Continue Demo Business. How can we help you today?',
    ],
  );
}

async function main(): Promise<void> {
  const config = loadConfig();
  const pool = createPool(config.databaseUrl);
  try {
    await seedDatabase(pool);
  } finally {
    await pool.end();
  }
}

if (process.argv[1]?.endsWith('seed.js') || process.argv[1]?.endsWith('seed.ts')) {
  await main();
}

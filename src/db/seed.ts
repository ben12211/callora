import type pg from 'pg';
import { ensureBootstrapAdmin } from '../auth/bootstrap.js';
import { loadConfig } from '../config.js';
import { createPool } from './pool.js';
import { PostgresStore } from './postgres-store.js';

const exampleBusinessId = '00000000-0000-4000-8000-000000000001';

export async function seedDatabase(pool: pg.Pool): Promise<void> {
  await pool.query(
    `INSERT INTO businesses (id, name, phone_number, greeting, active)
     VALUES ($1, $2, $3, $4, true)
     ON CONFLICT DO NOTHING`,
    [
      exampleBusinessId,
      'Callora Demo Business',
      '+15551234567',
      'Thanks for calling Callora Demo Business. How can we help you today?',
    ],
  );

  await pool.query(
    `INSERT INTO agent_configs (
       business_id, instructions, greeting, language, voice, realtime_model, enabled,
       voice_provider
     ) VALUES ($1, $2, $3, $4, $5, $6, true, $7)
     ON CONFLICT (business_id) DO NOTHING`,
    [
      exampleBusinessId,
      // Business-specific context only: tone, scope, and what this business offers.
      // Phone style, business-only scope, and hangup rules come from the global Callora
      // policy in src/realtime/policy.ts and cannot be overridden from here.
      [
        'העסק הוא "קלורה דמו", שירות הדגמה של מערכת מענה טלפוני חכם.',
        'דבר/י עברית טבעית ויומיומית, בנימה חמה ומקצועית.',
        'נושאים שבתחומך: מה המערכת עושה, איך מתחילים להשתמש בה, ותיאום שיחת המשך עם נציג.',
        'אל תמציא/י מידע על מחירים, מלאי או מועדים. אם אינך יודע/ת, אמור/י זאת והצע/י שנציג יחזור ללקוח.',
      ].join(' '),
      'שלום, הגעתם לקלורה דמו. איך אפשר לעזור?',
      'he-IL',
      'marin',
      'gpt-realtime-2.1',
      // The seeded demo answers on OpenAI Realtime; each business picks its own provider
      // from the dashboard afterwards.
      'openai',
    ],
  );
}

async function main(): Promise<void> {
  const config = loadConfig();
  const pool = createPool(config.databaseUrl);
  try {
    await seedDatabase(pool);
    // The dashboard is unusable without an account, so seeding creates the bootstrap
    // administrator too when the environment supplies one.
    await ensureBootstrapAdmin(new PostgresStore(pool), config.auth, {
      info: (details, msg) => process.stdout.write(`${JSON.stringify({ level: 'info', msg, ...details })}
`),
      warn: (details, msg) => process.stdout.write(`${JSON.stringify({ level: 'warn', msg, ...details })}
`),
    });
  } finally {
    await pool.end();
  }
}

if (process.argv[1]?.endsWith('seed.js') || process.argv[1]?.endsWith('seed.ts')) {
  await main();
}

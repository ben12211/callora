import type pg from 'pg';
import { loadConfig } from '../config.js';
import { createPool } from './pool.js';

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
       business_id, instructions, greeting, language, voice, realtime_model, enabled
     ) VALUES ($1, $2, $3, $4, $5, $6, true)
     ON CONFLICT (business_id) DO NOTHING`,
    [
      exampleBusinessId,
      [
        'את/ה נציג/ת שירות לקוחות טלפוני של "קלורה דמו".',
        'דבר/י עברית טבעית ויומיומית, בקצב רגיל ובנימה חמה ומקצועית.',
        'ענה/י בתשובות קצרות של משפט או שניים, כמו בשיחת טלפון אמיתית.',
        'אל תמציא/י מידע. אם אינך יודע/ת, אמור/י זאת בכנות והצע/י לברר ולחזור ללקוח.',
        'אם הלקוח קוטע אותך, הפסק/י לדבר מיד והקשב/י.',
      ].join(' '),
      'שלום, הגעתם לקלורה דמו. איך אפשר לעזור?',
      'he-IL',
      'marin',
      'gpt-realtime-2.1',
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

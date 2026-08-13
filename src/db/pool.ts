import pg from 'pg';

const { Pool } = pg;

export function createPool(connectionString: string): pg.Pool {
  const pool = new Pool({
    connectionString,
    max: 10,
    idleTimeoutMillis: 30_000,
    connectionTimeoutMillis: 5_000,
  });

  // Prevent an idle connection failure from terminating the process; future
  // queries will obtain a fresh connection after PostgreSQL recovers.
  pool.on('error', (error) => {
    process.stderr.write(`[database] Idle PostgreSQL connection failed: ${error.message}\n`);
  });

  return pool;
}

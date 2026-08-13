import { buildApp } from './app.js';
import { loadConfig } from './config.js';
import { createPool } from './db/pool.js';
import { PostgresStore } from './db/postgres-store.js';

const config = loadConfig();
const pool = createPool(config.databaseUrl);
const store = new PostgresStore(pool);
const app = await buildApp({ config, store });

async function shutdown(signal: string): Promise<void> {
  app.log.info({ signal }, 'Shutting down');
  await app.close();
  await pool.end();
  process.exit(0);
}

process.on('SIGTERM', () => void shutdown('SIGTERM'));
process.on('SIGINT', () => void shutdown('SIGINT'));

try {
  await app.listen({ host: config.host, port: config.port });
} catch (error) {
  app.log.fatal({ error }, 'Failed to start server');
  await pool.end();
  process.exit(1);
}

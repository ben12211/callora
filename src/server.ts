import { buildApp } from './app.js';
import { loadConfig } from './config.js';
import { createPool } from './db/pool.js';
import { PostgresStore } from './db/postgres-store.js';
import { loadCallerAllowlist } from './dev/caller-allowlist.js';

const config = loadConfig();
const pool = createPool(config.databaseUrl);
const store = new PostgresStore(pool);
// Read from the working directory, so the gitignored file sits at the project root in
// development and is simply absent in the container.
// The app logger does not exist yet, so emit the same JSON shape Fastify's pino uses.
const startupLog = (level: string) => (details: Record<string, unknown>, msg: string): void => {
  process.stdout.write(`${JSON.stringify({ level, msg, ...details })}\n`);
};
const callerAllowlist = await loadCallerAllowlist(process.cwd(), {
  info: startupLog('info'),
  warn: startupLog('warn'),
});
const app = await buildApp({ config, store, callerAllowlist });
let shuttingDown = false;

async function shutdown(signal: string): Promise<void> {
  if (shuttingDown) {
    return;
  }
  shuttingDown = true;
  app.log.info({ signal }, 'Shutting down');
  await app.close();
  await pool.end();
}

process.once('SIGTERM', () => void shutdown('SIGTERM'));
process.once('SIGINT', () => void shutdown('SIGINT'));

try {
  await app.listen({ host: config.host, port: config.port });
} catch (error) {
  app.log.fatal({ error }, 'Failed to start server');
  await pool.end();
  process.exit(1);
}

import { buildApp } from './app.js';
import { ensureBootstrapAdmin } from './auth/bootstrap.js';
import { loadConfig, missingProviderCredentials } from './config.js';
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
// The dashboard needs at least one account before anyone can sign in.
await ensureBootstrapAdmin(store, config.auth, {
  info: startupLog('info'),
  warn: startupLog('warn'),
});
// Missing credentials are no longer fatal: they can be entered in the dashboard, and any
// that are already stored there are merged in a moment. Say so once, loudly enough to
// explain why calls are answering with the static greeting on a fresh deployment.
const missing = missingProviderCredentials(process.env);
if (missing.length > 0) {
  startupLog('warn')(
    { voiceProvider: config.voiceProvider, missing },
    'The default provider has no credentials in the environment; set them here or in the dashboard under Providers',
  );
}
const app = await buildApp({ config, store, callerAllowlist });
// Expired sessions are already rejected on lookup; this only keeps the table from
// growing without bound.
const sessionSweep = setInterval(
  () => {
    void store.deleteExpiredAdminSessions().catch((error: unknown) => {
      app.log.warn({ error }, 'Failed to prune expired admin sessions');
    });
  },
  60 * 60 * 1000,
);
sessionSweep.unref();

/**
 * Retention sweep for conversation transcripts.
 *
 * They are the caller's own words, so they age out on a schedule rather than accumulating
 * forever. `TRANSCRIPT_RETENTION_DAYS=0` keeps them, which has to be a deliberate choice.
 */
const transcriptSweep =
  config.transcriptRetentionDays > 0
    ? setInterval(
        () => {
          const cutoff = new Date(Date.now() - config.transcriptRetentionDays * 24 * 60 * 60 * 1000);
          void store
            .deleteTranscriptsOlderThan(cutoff)
            .then((removed) => {
              if (removed > 0) {
                app.log.info({ removed, cutoff }, 'Pruned transcripts past their retention window');
              }
            })
            .catch((error: unknown) => {
              app.log.warn({ error }, 'Failed to prune transcripts');
            });
        },
        60 * 60 * 1000,
      )
    : null;
transcriptSweep?.unref();

let shuttingDown = false;

/**
 * How long a shutdown waits for live conversations to end on their own.
 *
 * All bridge state is in memory, so closing the server immediately drops every call
 * mid-sentence. Waiting costs a slower deploy; not waiting costs real conversations.
 */
const DRAIN_TIMEOUT_MS = Number(process.env['SHUTDOWN_DRAIN_TIMEOUT_MS'] ?? 30_000);

async function shutdown(signal: string): Promise<void> {
  if (shuttingDown) {
    return;
  }
  shuttingDown = true;
  clearInterval(sessionSweep);
  if (transcriptSweep) {
    clearInterval(transcriptSweep);
  }

  const active = app.callRegistry.size;
  app.log.info({ signal, activeCalls: active, drainTimeoutMs: DRAIN_TIMEOUT_MS }, 'Shutting down');

  // Marks the instance draining first, so `/health` starts failing and no new call is
  // routed here while the existing ones finish.
  const { drained, forced } = await app.callRegistry.drain(DRAIN_TIMEOUT_MS);
  if (active > 0) {
    app.log.info({ drained, forced }, 'Finished draining live calls');
  }

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

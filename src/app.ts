import formbody from '@fastify/formbody';
import websocket from '@fastify/websocket';
import Fastify, { type FastifyInstance } from 'fastify';
import { AuthService } from './auth/sessions.js';
import { AuditRecorder } from './http/audit.js';
import type { RouteDependencies } from './http/dependencies.js';
import { registerRoutes } from './http/routes.js';
import { MetricsRegistry } from './platform/metrics.js';
import { createSecretBox } from './platform/secret-box.js';
import { PlatformSettings, SETTINGS_REFRESH_INTERVAL_MS } from './platform/settings.js';
import { CallRegistry } from './telephony/call-registry.js';

declare module 'fastify' {
  interface FastifyInstance {
    /** Calls this instance is bridging right now; see `call-registry.ts`. */
    callRegistry: CallRegistry;
    /** Call-path counters, rendered at `/metrics`. */
    metrics: MetricsRegistry;
  }
}

type AppDependencies = RouteDependencies;

function databaseErrorCode(error: unknown): string | undefined {
  if (typeof error !== 'object' || error === null || !('code' in error)) {
    return undefined;
  }
  return typeof error.code === 'string' ? error.code : undefined;
}

function httpErrorStatus(error: unknown): number | undefined {
  if (typeof error !== 'object' || error === null || !('statusCode' in error)) {
    return undefined;
  }
  return typeof error.statusCode === 'number' ? error.statusCode : undefined;
}

export async function buildApp(dependencies: AppDependencies): Promise<FastifyInstance> {
  const { config } = dependencies;
  const app = Fastify({
    logger: config.nodeEnv === 'test' ? false : { level: config.logLevel },
    trustProxy: true,
  });

  await app.register(formbody);
  await app.register(websocket, {
    options: {
      // Twilio media frames are small JSON payloads; cap them so a rogue peer cannot
      // exhaust memory on the shared backend.
      maxPayload: 256 * 1024,
    },
  });
  // Dashboard pages carry tenant configuration behind a session cookie; a shared or
  // browser cache holding them would outlive the session that was allowed to see them.
  app.addHook('onSend', async (request, reply, payload) => {
    if (request.url.startsWith('/dashboard')) {
      void reply.header('cache-control', 'no-store');
      void reply.header('referrer-policy', 'same-origin');
      void reply.header('x-content-type-options', 'nosniff');
      void reply.header('x-frame-options', 'DENY');
    }
    return payload;
  });

  // Dashboard-managed settings are merged over the environment before any route reads a
  // credential. A database that cannot be read leaves the deployment on its environment.
  const platform =
    dependencies.platform ??
    new PlatformSettings({
      store: dependencies.store,
      config,
      secretBox: createSecretBox(config.secretsKey),
      logger: app.log,
      ...(dependencies.callerAllowlist ? { fallbackAllowlist: dependencies.callerAllowlist } : {}),
    });
  await platform.load();

  // The call path re-checks the settings as it uses them; this only keeps an idle
  // instance converging too, so a change made on one is visible on the others without
  // waiting for a call. Tests drive `refreshIfStale` directly instead.
  if (config.nodeEnv !== 'test') {
    const refresh = setInterval(() => {
      void platform.refreshIfStale(SETTINGS_REFRESH_INTERVAL_MS);
    }, SETTINGS_REFRESH_INTERVAL_MS);
    refresh.unref?.();
    app.addHook('onClose', async () => {
      clearInterval(refresh);
    });
  }

  const registry = dependencies.registry ?? new CallRegistry();
  const metrics = dependencies.metrics ?? new MetricsRegistry();
  // Read at scrape time from the registry itself, so the gauge cannot drift from reality.
  metrics.trackActiveCalls(() => registry.size);
  app.decorate('callRegistry', registry);
  app.decorate('metrics', metrics);

  await registerRoutes(app, {
    ...dependencies,
    registry,
    metrics,
    platform,
    auth: new AuthService(dependencies.store, config.auth),
    audit: new AuditRecorder(dependencies.store, app.log),
  });

  app.setErrorHandler((error, request, reply) => {
    const code = databaseErrorCode(error);
    if (code === '23505') {
      void reply.code(409).send({ error: 'A record with that unique value already exists' });
      return;
    }
    if (code === '23503') {
      void reply.code(409).send({ error: 'The record is referenced by another resource' });
      return;
    }

    const statusCode = httpErrorStatus(error);
    if (statusCode && statusCode >= 400 && statusCode < 500) {
      const message = error instanceof Error ? error.message : 'Invalid request';
      void reply.code(statusCode).send({ error: message });
      return;
    }

    request.log.error({ error }, 'Unhandled request error');
    void reply.code(500).send({ error: 'Internal server error' });
  });

  return app;
}

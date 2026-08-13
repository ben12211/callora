import formbody from '@fastify/formbody';
import websocket from '@fastify/websocket';
import Fastify, { type FastifyInstance } from 'fastify';
import type { AppConfig } from './config.js';
import type { DataStore } from './db/store.js';
import { registerRoutes } from './http/routes.js';
import type { CallTerminator } from './telephony/call-terminator.js';

interface AppDependencies {
  config: AppConfig;
  store: DataStore;
  /** Overridable so tests never reach the Twilio REST API. */
  callTerminator?: CallTerminator;
}

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
  await registerRoutes(app, dependencies);

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

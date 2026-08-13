import formbody from '@fastify/formbody';
import Fastify, { type FastifyInstance } from 'fastify';
import type { AppConfig } from './config.js';
import type { DataStore } from './db/store.js';
import { registerRoutes } from './http/routes.js';

interface AppDependencies {
  config: AppConfig;
  store: DataStore;
}

function databaseErrorCode(error: unknown): string | undefined {
  if (typeof error !== 'object' || error === null || !('code' in error)) {
    return undefined;
  }
  return typeof error.code === 'string' ? error.code : undefined;
}

export async function buildApp(dependencies: AppDependencies): Promise<FastifyInstance> {
  const { config } = dependencies;
  const app = Fastify({
    logger: config.nodeEnv === 'test' ? false : { level: config.logLevel },
    trustProxy: true,
  });

  await app.register(formbody);
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

    request.log.error({ error }, 'Unhandled request error');
    void reply.code(500).send({ error: 'Internal server error' });
  });

  return app;
}

import type { FastifyReply, FastifyRequest, preHandlerHookHandler } from 'fastify';
import twilio from 'twilio';
import type { AppConfig } from '../config.js';

function stringBody(body: unknown): Record<string, string> | null {
  if (!body || typeof body !== 'object' || Array.isArray(body)) {
    return null;
  }

  const result: Record<string, string> = {};
  for (const [key, value] of Object.entries(body)) {
    if (typeof value !== 'string') {
      return null;
    }
    result[key] = value;
  }
  return result;
}

export function twilioSignatureGuard(config: AppConfig): preHandlerHookHandler {
  return async (request: FastifyRequest, reply: FastifyReply): Promise<void> => {
    const signatureHeader = request.headers['x-twilio-signature'];
    const signature = Array.isArray(signatureHeader) ? signatureHeader[0] : signatureHeader;
    const body = stringBody(request.body);

    if (!signature || !body) {
      await reply.code(403).send({ error: 'Invalid Twilio signature' });
      return;
    }

    const webhookUrl = `${config.publicBaseUrl}${request.raw.url ?? request.url}`;
    const valid = twilio.validateRequest(config.twilioAuthToken, signature, webhookUrl, body);

    if (!valid) {
      await reply.code(403).send({ error: 'Invalid Twilio signature' });
    }
  };
}

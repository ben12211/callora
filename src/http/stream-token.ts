import { createHmac, timingSafeEqual } from 'node:crypto';

/**
 * Twilio does not sign the Media Streams WebSocket handshake, so Callora issues a
 * short-lived, call-scoped token from the already signature-validated voice webhook and
 * validates it on the upgrade request. The token binds a stream to exactly one call and
 * one business, which keeps tenants isolated even if a stream URL leaks.
 */
const TOKEN_LABEL = 'callora:media-stream:v1';

export interface StreamTokenPayload {
  callSid: string;
  businessId: string;
  expiresAt: number;
}

function base64url(value: Buffer): string {
  return value.toString('base64url');
}

function sign(secret: string, encodedPayload: string): string {
  return base64url(createHmac('sha256', secret).update(`${TOKEN_LABEL}.${encodedPayload}`).digest());
}

export function createStreamToken(
  secret: string,
  payload: Omit<StreamTokenPayload, 'expiresAt'>,
  ttlSeconds = 300,
  now: number = Date.now(),
): string {
  const body: StreamTokenPayload = {
    ...payload,
    expiresAt: Math.floor(now / 1000) + ttlSeconds,
  };
  const encodedPayload = base64url(Buffer.from(JSON.stringify(body), 'utf8'));
  return `${encodedPayload}.${sign(secret, encodedPayload)}`;
}

export function verifyStreamToken(
  secret: string,
  token: string,
  now: number = Date.now(),
): StreamTokenPayload | null {
  const separator = token.indexOf('.');
  if (separator <= 0) {
    return null;
  }

  const encodedPayload = token.slice(0, separator);
  const providedSignature = Buffer.from(token.slice(separator + 1), 'utf8');
  const expectedSignature = Buffer.from(sign(secret, encodedPayload), 'utf8');
  if (
    providedSignature.length !== expectedSignature.length ||
    !timingSafeEqual(providedSignature, expectedSignature)
  ) {
    return null;
  }

  let parsed: unknown;
  try {
    parsed = JSON.parse(Buffer.from(encodedPayload, 'base64url').toString('utf8'));
  } catch {
    return null;
  }

  if (typeof parsed !== 'object' || parsed === null) {
    return null;
  }
  const { callSid, businessId, expiresAt } = parsed as Record<string, unknown>;
  if (typeof callSid !== 'string' || typeof businessId !== 'string' || typeof expiresAt !== 'number') {
    return null;
  }
  if (expiresAt * 1000 <= now) {
    return null;
  }

  return { callSid, businessId, expiresAt };
}

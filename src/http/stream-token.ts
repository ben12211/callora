import { createHmac, timingSafeEqual } from 'node:crypto';

/**
 * In addition to Twilio's signed WebSocket handshake, Callora issues a short-lived,
 * call-scoped token from the signature-validated voice webhook. Twilio delivers it as
 * a custom Stream parameter in the start event. The token binds the stream to exactly
 * one call and one business so tenants remain isolated.
 *
 * Verification takes a list of secrets rather than one, so `STREAM_TOKEN_SECRET` can be
 * introduced or rotated without rejecting the tokens already in flight. Tokens are always
 * minted with the first entry.
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

function signatureMatches(secrets: readonly string[], encodedPayload: string, provided: string): boolean {
  const providedSignature = Buffer.from(provided, 'utf8');
  let matched = false;
  // Every candidate is checked, so the time taken does not reveal which one matched.
  for (const secret of secrets) {
    const expected = Buffer.from(sign(secret, encodedPayload), 'utf8');
    if (providedSignature.length === expected.length && timingSafeEqual(providedSignature, expected)) {
      matched = true;
    }
  }
  return matched;
}

export function verifyStreamToken(
  secret: string | readonly string[],
  token: string,
  now: number = Date.now(),
): StreamTokenPayload | null {
  const secrets = typeof secret === 'string' ? [secret] : secret;
  const separator = token.indexOf('.');
  if (separator <= 0) {
    return null;
  }

  const encodedPayload = token.slice(0, separator);
  if (!signatureMatches(secrets, encodedPayload, token.slice(separator + 1))) {
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

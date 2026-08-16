import type { FastifyReply, FastifyRequest } from 'fastify';

/**
 * Minimal cookie handling for the single session cookie the dashboard needs. A cookie
 * plugin would be the only dependency added for one name, so the two directions are
 * spelled out here instead.
 */
export function readCookie(request: FastifyRequest, name: string): string | undefined {
  const header = request.headers.cookie;
  if (!header) {
    return undefined;
  }
  for (const part of header.split(';')) {
    const separator = part.indexOf('=');
    if (separator === -1) {
      continue;
    }
    if (part.slice(0, separator).trim() === name) {
      return decodeURIComponent(part.slice(separator + 1).trim());
    }
  }
  return undefined;
}

export interface CookieOptions {
  maxAgeSeconds?: number;
  secure: boolean;
}

export function setCookie(reply: FastifyReply, name: string, value: string, options: CookieOptions): void {
  const attributes = [
    `${name}=${encodeURIComponent(value)}`,
    'Path=/',
    'HttpOnly',
    // Lax still sends the cookie on top-level navigation back from a bookmark or a link,
    // while keeping it off cross-site form posts.
    'SameSite=Lax',
  ];
  if (options.maxAgeSeconds !== undefined) {
    attributes.push(`Max-Age=${options.maxAgeSeconds}`);
  }
  if (options.secure) {
    attributes.push('Secure');
  }
  appendSetCookie(reply, attributes.join('; '));
}

export function clearCookie(reply: FastifyReply, name: string, options: CookieOptions): void {
  const attributes = [`${name}=`, 'Path=/', 'HttpOnly', 'SameSite=Lax', 'Max-Age=0'];
  if (options.secure) {
    attributes.push('Secure');
  }
  appendSetCookie(reply, attributes.join('; '));
}

function appendSetCookie(reply: FastifyReply, cookie: string): void {
  const existing = reply.getHeader('set-cookie');
  if (existing === undefined) {
    void reply.header('set-cookie', cookie);
    return;
  }
  const list = Array.isArray(existing) ? existing : [String(existing)];
  void reply.header('set-cookie', [...list, cookie]);
}

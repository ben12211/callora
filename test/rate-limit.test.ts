import { describe, expect, it } from 'vitest';
import { API_RATE_LIMIT, LOGIN_RATE_LIMIT, RateLimiter } from '../src/http/rate-limit.js';

/**
 * Sign-in timing was already equalized between an unknown address and a wrong password,
 * but nothing bounded the number of guesses, and the management API had no volume limit
 * at all.
 */

describe('RateLimiter', () => {
  it('allows up to the limit and then refuses', () => {
    const limiter = new RateLimiter({ limit: 3, windowMs: 60_000 });
    const now = 1_000;

    expect(limiter.check('1.2.3.4', now).allowed).toBe(true);
    expect(limiter.check('1.2.3.4', now).allowed).toBe(true);
    expect(limiter.check('1.2.3.4', now).allowed).toBe(true);

    const blocked = limiter.check('1.2.3.4', now);
    expect(blocked.allowed).toBe(false);
    expect(blocked.retryAfterSeconds).toBe(60);
  });

  it('keys clients separately', () => {
    const limiter = new RateLimiter({ limit: 1, windowMs: 60_000 });
    expect(limiter.check('1.2.3.4').allowed).toBe(true);
    expect(limiter.check('1.2.3.4').allowed).toBe(false);
    expect(limiter.check('5.6.7.8').allowed).toBe(true);
  });

  it('starts a fresh window once the old one expires', () => {
    const limiter = new RateLimiter({ limit: 1, windowMs: 60_000 });
    expect(limiter.check('1.2.3.4', 0).allowed).toBe(true);
    expect(limiter.check('1.2.3.4', 30_000).allowed).toBe(false);
    expect(limiter.check('1.2.3.4', 60_001).allowed).toBe(true);
  });

  it('clears the count on a success, so a typo never locks anyone out', () => {
    const limiter = new RateLimiter({ limit: 2, windowMs: 60_000 });
    limiter.check('1.2.3.4');
    limiter.check('1.2.3.4');
    expect(limiter.check('1.2.3.4').allowed).toBe(false);

    limiter.reset('1.2.3.4');
    expect(limiter.check('1.2.3.4').allowed).toBe(true);
  });

  it('stays bounded when an attacker rotates the key', () => {
    const limiter = new RateLimiter({ limit: 5, windowMs: 60_000, maxKeys: 50 });
    for (let index = 0; index < 5_000; index += 1) {
      limiter.check(`10.0.0.${index}`);
    }
    expect(limiter.size).toBeLessThanOrEqual(50);
  });

  it('configures sign-in tightly and the API loosely', () => {
    expect(LOGIN_RATE_LIMIT.limit).toBeLessThan(API_RATE_LIMIT.limit);
    expect(LOGIN_RATE_LIMIT.windowMs).toBeGreaterThan(API_RATE_LIMIT.windowMs);
  });
});

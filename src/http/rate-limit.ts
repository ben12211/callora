/**
 * Request throttling for the credential-checking surfaces.
 *
 * Sign-in timing is already equalized between an unknown address and a wrong password,
 * but nothing limited how many guesses could be made: a scrypt-hashed password is only as
 * safe as the number of attempts an attacker gets. The management API had no volume limit
 * of any kind.
 *
 * Deliberately in-process, so it needs no shared store and no new dependency. That means
 * the effective limit across N replicas is N times the configured one — enough to turn
 * unlimited online guessing into a rate an operator will see in the metrics and the logs,
 * not a distributed rate limiter. Twilio's webhooks stay outside this: they are already
 * authenticated by signature and legitimate call volume must never be throttled.
 */

export interface RateLimitDecision {
  allowed: boolean;
  /** Seconds until the next attempt would be allowed; only meaningful when blocked. */
  retryAfterSeconds: number;
  remaining: number;
}

export interface RateLimiterOptions {
  /** Attempts allowed inside one window. */
  limit: number;
  windowMs: number;
  /** Bound on distinct keys held, so an attacker rotating keys cannot exhaust memory. */
  maxKeys?: number;
}

const DEFAULT_MAX_KEYS = 10_000;

/**
 * Fixed-window counter, keyed by whatever the caller considers one client.
 *
 * A fixed window lets a burst straddle the boundary; that is an accepted trade for
 * keeping this small and allocation-free per request. The point is bounding guesses per
 * minute, not smoothing traffic.
 */
export class RateLimiter {
  private readonly windows = new Map<string, { count: number; resetAt: number }>();

  public constructor(private readonly options: RateLimiterOptions) {}

  public check(key: string, now = Date.now()): RateLimitDecision {
    const existing = this.windows.get(key);
    if (!existing || existing.resetAt <= now) {
      this.evictIfNeeded(now);
      this.windows.set(key, { count: 1, resetAt: now + this.options.windowMs });
      return { allowed: true, retryAfterSeconds: 0, remaining: this.options.limit - 1 };
    }

    existing.count += 1;
    if (existing.count > this.options.limit) {
      return {
        allowed: false,
        retryAfterSeconds: Math.max(1, Math.ceil((existing.resetAt - now) / 1000)),
        remaining: 0,
      };
    }
    return {
      allowed: true,
      retryAfterSeconds: 0,
      remaining: this.options.limit - existing.count,
    };
  }

  /** Called after a success, so a legitimate user is never locked out by their own typos. */
  public reset(key: string): void {
    this.windows.delete(key);
  }

  public get size(): number {
    return this.windows.size;
  }

  private evictIfNeeded(now: number): void {
    const maxKeys = this.options.maxKeys ?? DEFAULT_MAX_KEYS;
    if (this.windows.size < maxKeys) {
      return;
    }
    // Expired entries first; they cost nothing to drop.
    for (const [key, window] of this.windows) {
      if (window.resetAt <= now) {
        this.windows.delete(key);
      }
    }
    // Still full: drop the oldest insertions, which Map iterates first.
    while (this.windows.size >= maxKeys) {
      const oldest = this.windows.keys().next();
      if (oldest.done) {
        return;
      }
      this.windows.delete(oldest.value);
    }
  }
}

/** Sign-in: slow enough to make online guessing pointless, loose enough for a real typo. */
export const LOGIN_RATE_LIMIT: RateLimiterOptions = { limit: 10, windowMs: 5 * 60 * 1000 };

/** Management API: generous for scripts and dashboards, bounded against a runaway loop. */
export const API_RATE_LIMIT: RateLimiterOptions = { limit: 600, windowMs: 60 * 1000 };

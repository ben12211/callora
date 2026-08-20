import { describe, expect, it, vi } from 'vitest';
import { fallbackToGreeting } from '../src/http/media-stream.js';
import { CallRegistry } from '../src/telephony/call-registry.js';

/**
 * Two failure modes that used to be silent, in both senses.
 *
 * A `<Connect><Stream>` call whose stream closes hears nothing at all, so every path that
 * could not open a bridge left the caller in dead air. And because all bridge state is in
 * memory, a deploy closed the server underneath every live conversation.
 */

describe('CallRegistry', () => {
  const call = (callSid: string, close = (): void => {}) => ({
    businessId: 'b1',
    callSid,
    provider: 'openai',
    startedAt: Date.now(),
    close,
  });

  it('reports what the instance is carrying', () => {
    const registry = new CallRegistry();
    expect(registry.snapshot()).toEqual({ active: 0, draining: false, oldestStartedAt: null });

    registry.add(call('CA1'));
    registry.add(call('CA2'));
    expect(registry.size).toBe(2);
    expect(registry.snapshot().active).toBe(2);

    registry.remove('CA1');
    expect(registry.size).toBe(1);
  });

  it('resolves as soon as the last call ends on its own', async () => {
    vi.useFakeTimers();
    try {
      const registry = new CallRegistry();
      const forced = vi.fn();
      registry.add(call('CA1', forced));

      const drain = registry.drain(30_000);
      expect(registry.isDraining).toBe(true);

      // The caller hangs up two seconds in, well inside the window.
      vi.advanceTimersByTime(2_000);
      registry.remove('CA1');

      await expect(drain).resolves.toEqual({ drained: 1, forced: 0 });
      expect(forced).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });

  it('closes whatever is still connected when the window runs out', async () => {
    vi.useFakeTimers();
    try {
      const registry = new CallRegistry();
      const forced = vi.fn();
      registry.add(call('CA1', forced));

      const drain = registry.drain(30_000);
      vi.advanceTimersByTime(30_000);

      await expect(drain).resolves.toEqual({ drained: 0, forced: 1 });
      expect(forced).toHaveBeenCalledExactlyOnceWith('server-shutdown');
    } finally {
      vi.useRealTimers();
    }
  });

  it('returns immediately when there is nothing to drain', async () => {
    const registry = new CallRegistry();
    await expect(registry.drain(30_000)).resolves.toEqual({ drained: 0, forced: 0 });
    expect(registry.isDraining).toBe(true);
  });
});

describe('a caller never gets dead air', () => {
  const businessId = '00000000-0000-4000-8000-000000000001';
  const callSid = 'CADEGRADE1';

  function harness(greeting: string | null) {
    const sayAndHangUp = vi.fn(async () => {});
    const endCall = vi.fn(async () => {});
    const logged: string[] = [];
    return {
      sayAndHangUp,
      endCall,
      logged,
      deps: {
        store: {
          getBusinessById: async () => (greeting === null ? null : { id: businessId, greeting }),
        },
        callTerminator: { sayAndHangUp, endCall },
      },
      app: {
        log: {
          info: (_d: unknown, message: string) => logged.push(message),
          warn: (_d: unknown, message: string) => logged.push(message),
          error: (_d: unknown, message: string) => logged.push(message),
        },
      },
    };
  }

  it('speaks the business greeting when the bridge cannot be opened', async () => {
    const { deps, app, sayAndHangUp, endCall } = harness('Thanks for calling Acme.');

    await fallbackToGreeting(
      app as never,
      deps as never,
      businessId,
      callSid,
      'bridge-failed',
    );

    expect(sayAndHangUp).toHaveBeenCalledExactlyOnceWith(callSid, 'Thanks for calling Acme.');
    expect(endCall).not.toHaveBeenCalled();
  });

  it('hangs up cleanly when there is no greeting to speak', async () => {
    const { deps, app, sayAndHangUp, endCall } = harness('   ');

    await fallbackToGreeting(app as never, deps as never, businessId, callSid, 'bridge-failed');

    expect(sayAndHangUp).not.toHaveBeenCalled();
    expect(endCall).toHaveBeenCalledExactlyOnceWith(callSid);
  });

  it('never lets the fallback itself throw into the stream handler', async () => {
    const { deps, app, logged } = harness('Thanks for calling Acme.');
    deps.callTerminator.sayAndHangUp = vi.fn(async () => {
      throw new Error('twilio unavailable');
    });

    await expect(
      fallbackToGreeting(app as never, deps as never, businessId, callSid, 'bridge-failed'),
    ).resolves.toBeUndefined();
    expect(logged).toContain('Failed to fall back to the static greeting');
  });
});

/**
 * The calls this process is currently bridging.
 *
 * All bridge state lives in memory, so a restart that simply closes the server drops
 * every conversation mid-sentence. The registry is what lets a deploy wait for calls to
 * finish on their own instead, and what lets `/health` and the metrics endpoint report
 * how much live traffic an instance is actually carrying.
 */

export interface ActiveCall {
  businessId: string;
  callSid: string;
  provider: string;
  startedAt: number;
  /** Ends this call early, used when a drain runs out of patience. */
  close: (reason: string) => void;
}

export interface CallRegistrySnapshot {
  active: number;
  draining: boolean;
  oldestStartedAt: number | null;
}

export class CallRegistry {
  private readonly calls = new Map<string, ActiveCall>();
  private draining = false;
  private readonly waiters = new Set<() => void>();

  /** True once a shutdown has begun; new streams are refused from that point on. */
  public get isDraining(): boolean {
    return this.draining;
  }

  public get size(): number {
    return this.calls.size;
  }

  public add(call: ActiveCall): void {
    this.calls.set(call.callSid, call);
  }

  public remove(callSid: string): void {
    if (!this.calls.delete(callSid)) {
      return;
    }
    if (this.calls.size === 0) {
      for (const wake of this.waiters) {
        wake();
      }
    }
  }

  public snapshot(): CallRegistrySnapshot {
    let oldest: number | null = null;
    for (const call of this.calls.values()) {
      if (oldest === null || call.startedAt < oldest) {
        oldest = call.startedAt;
      }
    }
    return { active: this.calls.size, draining: this.draining, oldestStartedAt: oldest };
  }

  public list(): ActiveCall[] {
    return [...this.calls.values()];
  }

  /**
   * Stops accepting new streams and waits for the live ones to end on their own, up to
   * `timeoutMs`. Whatever is still connected after that is closed, because the process
   * is going away regardless and a clean close at least ends the Twilio call properly.
   */
  public async drain(timeoutMs: number): Promise<{ drained: number; forced: number }> {
    this.draining = true;
    const drained = this.calls.size;
    if (drained === 0) {
      return { drained: 0, forced: 0 };
    }

    await new Promise<void>((resolve) => {
      const timer = setTimeout(() => {
        this.waiters.delete(wake);
        resolve();
      }, timeoutMs);
      timer.unref?.();

      const wake = (): void => {
        clearTimeout(timer);
        this.waiters.delete(wake);
        resolve();
      };
      this.waiters.add(wake);
    });

    const remaining = this.list();
    for (const call of remaining) {
      call.close('server-shutdown');
    }
    return { drained: drained - remaining.length, forced: remaining.length };
  }
}

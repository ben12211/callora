export interface ReNikudResult {
  pronunciation: string;
  processingMs: number;
}

export interface ReNikudClientOptions {
  baseUrl: string;
  timeoutMs: number;
  fetchImpl?: typeof fetch;
}

/** Failure is represented as null: this optional enhancement never owns call availability. */
export class ReNikudClient {
  public constructor(private readonly options: ReNikudClientOptions) {}

  public async pronounce(
    text: string,
    genders: { speakerGender?: 'male' | 'female'; targetGender?: 'male' | 'female' } = {},
    parentSignal?: AbortSignal,
  ): Promise<ReNikudResult | null> {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), this.options.timeoutMs);
    timer.unref?.();
    const abort = (): void => controller.abort();
    parentSignal?.addEventListener('abort', abort, { once: true });
    try {
      const response = await (this.options.fetchImpl ?? fetch)(`${this.options.baseUrl}/pronounce`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ text, ...genders }),
        signal: controller.signal,
      });
      if (!response.ok) return null;
      const value: unknown = await response.json();
      if (!value || typeof value !== 'object') return null;
      const pronunciation = (value as Record<string, unknown>)['pronunciation'];
      const processingMs = (value as Record<string, unknown>)['processingMs'];
      if (typeof pronunciation !== 'string' || !pronunciation.trim() || typeof processingMs !== 'number') return null;
      return { pronunciation: pronunciation.trim(), processingMs };
    } catch {
      return null;
    } finally {
      clearTimeout(timer);
      parentSignal?.removeEventListener('abort', abort);
    }
  }
}


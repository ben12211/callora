/**
 * In-process metrics for the call path.
 *
 * Callora is a realtime voice product whose observability was log lines only: nothing
 * recorded how many calls an instance was carrying, how long the caller waited before
 * hearing anything, how often a provider failed to connect, or why calls ended. Those are
 * exactly the numbers that decide whether the product is working.
 *
 * Deliberately dependency-free and exposed in the Prometheus text format, which every
 * scraper and most dashboards already read. Counters are per process and reset with it,
 * which is what a scraper expects.
 */

/** Buckets in milliseconds. A caller notices roughly half a second of silence. */
const LATENCY_BUCKETS_MS = [100, 250, 500, 1_000, 2_000, 5_000, 10_000] as const;

/** Buckets in seconds, from a wrong number to a long support conversation. */
const DURATION_BUCKETS_S = [5, 15, 30, 60, 120, 300, 600, 1_800, 3_600] as const;

type Labels = Record<string, string>;

function labelKey(labels: Labels): string {
  return Object.keys(labels)
    .sort()
    .map((name) => `${name}=${labels[name] ?? ''}`)
    .join(',');
}

function renderLabels(labels: Labels): string {
  const entries = Object.entries(labels).sort(([a], [b]) => a.localeCompare(b));
  if (entries.length === 0) {
    return '';
  }
  const rendered = entries
    .map(([name, value]) => `${name}="${value.replace(/\\/g, '\\\\').replace(/"/g, '\\"')}"`)
    .join(',');
  return `{${rendered}}`;
}

class Counter {
  private readonly values = new Map<string, { labels: Labels; value: number }>();

  public constructor(
    public readonly name: string,
    public readonly help: string,
  ) {}

  public increment(labels: Labels = {}, by = 1): void {
    const key = labelKey(labels);
    const existing = this.values.get(key);
    if (existing) {
      existing.value += by;
      return;
    }
    this.values.set(key, { labels, value: by });
  }

  public render(): string[] {
    const lines = [`# HELP ${this.name} ${this.help}`, `# TYPE ${this.name} counter`];
    for (const { labels, value } of this.values.values()) {
      lines.push(`${this.name}${renderLabels(labels)} ${value}`);
    }
    return lines;
  }
}

class Histogram {
  private readonly series = new Map<
    string,
    { labels: Labels; counts: number[]; sum: number; count: number }
  >();

  public constructor(
    public readonly name: string,
    public readonly help: string,
    private readonly buckets: readonly number[],
  ) {}

  public observe(value: number, labels: Labels = {}): void {
    const key = labelKey(labels);
    let entry = this.series.get(key);
    if (!entry) {
      entry = { labels, counts: new Array<number>(this.buckets.length).fill(0), sum: 0, count: 0 };
      this.series.set(key, entry);
    }
    entry.sum += value;
    entry.count += 1;
    for (let index = 0; index < this.buckets.length; index += 1) {
      if (value <= (this.buckets[index] ?? Number.POSITIVE_INFINITY)) {
        entry.counts[index] = (entry.counts[index] ?? 0) + 1;
      }
    }
  }

  public render(): string[] {
    const lines = [`# HELP ${this.name} ${this.help}`, `# TYPE ${this.name} histogram`];
    for (const entry of this.series.values()) {
      let cumulative = 0;
      for (let index = 0; index < this.buckets.length; index += 1) {
        cumulative = entry.counts[index] ?? 0;
        lines.push(
          `${this.name}_bucket${renderLabels({ ...entry.labels, le: String(this.buckets[index]) })} ${cumulative}`,
        );
      }
      lines.push(`${this.name}_bucket${renderLabels({ ...entry.labels, le: '+Inf' })} ${entry.count}`);
      lines.push(`${this.name}_sum${renderLabels(entry.labels)} ${entry.sum}`);
      lines.push(`${this.name}_count${renderLabels(entry.labels)} ${entry.count}`);
    }
    return lines;
  }
}

/**
 * What a bridge reports while a call is running.
 *
 * Passed into every provider bridge so one shape covers all three, and optional so tests
 * and any future caller can leave it out.
 */
export interface CallMetrics {
  /** The caller heard the agent for the first time, measured from the stream opening. */
  firstAudio(provider: string, latencyMs: number): void;
  /** The caller talked over the agent. */
  bargeIn(provider: string): void;
}

export class MetricsRegistry implements CallMetrics {
  private readonly callsStarted = new Counter(
    'callora_calls_started_total',
    'Media streams this process began bridging.',
  );
  private readonly callsEnded = new Counter(
    'callora_calls_ended_total',
    'Bridged calls that finished, labelled by why.',
  );
  private readonly fallbacks = new Counter(
    'callora_greeting_fallbacks_total',
    'Calls handed back to the static greeting because the realtime path was unavailable.',
  );
  private readonly bargeIns = new Counter(
    'callora_barge_ins_total',
    'Times a caller talked over the agent.',
  );
  private readonly firstAudioLatency = new Histogram(
    'callora_first_audio_latency_ms',
    'Milliseconds between the media stream opening and the caller hearing the agent.',
    LATENCY_BUCKETS_MS,
  );
  private readonly callDuration = new Histogram(
    'callora_call_duration_seconds',
    'How long a bridged call lasted.',
    DURATION_BUCKETS_S,
  );

  /** Read at scrape time rather than tracked, so it cannot drift from the registry. */
  private activeCalls: () => number = () => 0;

  public trackActiveCalls(read: () => number): void {
    this.activeCalls = read;
  }

  public callStarted(provider: string): void {
    this.callsStarted.increment({ provider });
  }

  public callEnded(provider: string, reason: string, durationSeconds: number): void {
    this.callsEnded.increment({ provider, reason });
    this.callDuration.observe(durationSeconds, { provider });
  }

  public greetingFallback(reason: string): void {
    this.fallbacks.increment({ reason });
  }

  public firstAudio(provider: string, latencyMs: number): void {
    this.firstAudioLatency.observe(latencyMs, { provider });
  }

  public bargeIn(provider: string): void {
    this.bargeIns.increment({ provider });
  }

  public render(): string {
    const lines = [
      '# HELP callora_calls_active Calls this instance is bridging right now.',
      '# TYPE callora_calls_active gauge',
      `callora_calls_active ${this.activeCalls()}`,
      ...this.callsStarted.render(),
      ...this.callsEnded.render(),
      ...this.fallbacks.render(),
      ...this.bargeIns.render(),
      ...this.firstAudioLatency.render(),
      ...this.callDuration.render(),
    ];
    return `${lines.join('\n')}\n`;
  }
}

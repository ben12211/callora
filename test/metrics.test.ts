import { describe, expect, it } from 'vitest';
import { MetricsRegistry } from '../src/platform/metrics.js';

/**
 * The call path used to be observable through log lines only, so nothing recorded how
 * long a caller waited before hearing anything, how often a provider failed, or why calls
 * ended — the numbers that decide whether a voice product is working.
 */

function metricLine(rendered: string, prefix: string): string | undefined {
  return rendered.split('\n').find((line) => line.startsWith(prefix));
}

describe('MetricsRegistry', () => {
  it('reports live calls from the registry rather than its own tally', () => {
    const metrics = new MetricsRegistry();
    let active = 3;
    metrics.trackActiveCalls(() => active);

    expect(metricLine(metrics.render(), 'callora_calls_active ')).toBe('callora_calls_active 3');

    active = 0;
    expect(metricLine(metrics.render(), 'callora_calls_active ')).toBe('callora_calls_active 0');
  });

  it('counts calls per provider and end reason', () => {
    const metrics = new MetricsRegistry();
    metrics.callStarted('openai');
    metrics.callStarted('openai');
    metrics.callStarted('cartesia');
    metrics.callEnded('openai', '1000', 42);
    metrics.callEnded('openai', '1011', 3);

    const rendered = metrics.render();
    expect(metricLine(rendered, 'callora_calls_started_total{provider="openai"}')).toBe(
      'callora_calls_started_total{provider="openai"} 2',
    );
    expect(metricLine(rendered, 'callora_calls_started_total{provider="cartesia"}')).toBe(
      'callora_calls_started_total{provider="cartesia"} 1',
    );
    expect(metricLine(rendered, 'callora_calls_ended_total{provider="openai",reason="1011"}')).toBe(
      'callora_calls_ended_total{provider="openai",reason="1011"} 1',
    );
  });

  it('records how long the caller waited to hear anything', () => {
    const metrics = new MetricsRegistry();
    metrics.firstAudio('openai', 320);
    metrics.firstAudio('openai', 1_400);

    const rendered = metrics.render();
    // 320ms lands in the 500ms bucket; 1400ms does not.
    expect(metricLine(rendered, 'callora_first_audio_latency_ms_bucket{le="500",provider="openai"}')).toBe(
      'callora_first_audio_latency_ms_bucket{le="500",provider="openai"} 1',
    );
    expect(metricLine(rendered, 'callora_first_audio_latency_ms_bucket{le="2000",provider="openai"}')).toBe(
      'callora_first_audio_latency_ms_bucket{le="2000",provider="openai"} 2',
    );
    expect(metricLine(rendered, 'callora_first_audio_latency_ms_count{provider="openai"}')).toBe(
      'callora_first_audio_latency_ms_count{provider="openai"} 2',
    );
    expect(metricLine(rendered, 'callora_first_audio_latency_ms_sum{provider="openai"}')).toBe(
      'callora_first_audio_latency_ms_sum{provider="openai"} 1720',
    );
  });

  it('counts barge-ins and greeting fallbacks', () => {
    const metrics = new MetricsRegistry();
    metrics.bargeIn('elevenlabs');
    metrics.bargeIn('elevenlabs');
    metrics.greetingFallback('provider-unavailable');

    const rendered = metrics.render();
    expect(metricLine(rendered, 'callora_barge_ins_total{provider="elevenlabs"}')).toBe(
      'callora_barge_ins_total{provider="elevenlabs"} 2',
    );
    expect(
      metricLine(rendered, 'callora_greeting_fallbacks_total{reason="provider-unavailable"}'),
    ).toBe('callora_greeting_fallbacks_total{reason="provider-unavailable"} 1');
  });

  it('renders a scrapeable document even with nothing recorded', () => {
    const rendered = new MetricsRegistry().render();
    expect(rendered).toContain('# TYPE callora_calls_active gauge');
    expect(rendered).toContain('# TYPE callora_calls_started_total counter');
    expect(rendered.endsWith('\n')).toBe(true);
  });
});

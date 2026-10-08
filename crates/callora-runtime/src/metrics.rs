//! Process metrics in the Prometheus text format. Hand-rolled: a handful of counters and
//! fixed-bucket histograms is all the latency targets need, and it keeps the hot path to
//! a few atomic adds.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

const BUCKETS_MS: [u64; 14] = [50, 100, 150, 200, 250, 300, 350, 450, 650, 900, 1300, 2000, 3500, 6000];

#[derive(Default)]
pub struct Histogram {
    buckets: [AtomicU64; 14],
    count: AtomicU64,
    sum: AtomicU64,
}

impl Histogram {
    pub fn observe(&self, ms: u64) {
        for (i, b) in BUCKETS_MS.iter().enumerate() {
            if ms <= *b {
                self.buckets[i].fetch_add(1, Ordering::Relaxed);
            }
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum.fetch_add(ms, Ordering::Relaxed);
    }

    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    fn render(&self, name: &str, help: &str, labels: &str, out: &mut String) {
        out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} histogram\n"));
        let sep = if labels.is_empty() { "" } else { "," };
        for (i, b) in BUCKETS_MS.iter().enumerate() {
            out.push_str(&format!(
                "{name}_bucket{{{labels}{sep}le=\"{b}\"}} {}\n",
                self.buckets[i].load(Ordering::Relaxed)
            ));
        }
        let count = self.count.load(Ordering::Relaxed);
        out.push_str(&format!("{name}_bucket{{{labels}{sep}le=\"+Inf\"}} {count}\n"));
        let l = if labels.is_empty() { String::new() } else { format!("{{{labels}}}") };
        out.push_str(&format!("{name}_sum{l} {}\n{name}_count{l} {count}\n", self.sum.load(Ordering::Relaxed)));
    }
}

/// A counter split by one label (`reason`, `kind`): barge-ins by what stopped the agent.
#[derive(Default)]
pub struct LabeledCounter {
    values: Mutex<BTreeMap<&'static str, u64>>,
}

impl LabeledCounter {
    pub fn inc(&self, label: &'static str) {
        *self.values.lock().entry(label).or_default() += 1;
    }

    pub fn get(&self, label: &str) -> u64 {
        self.values.lock().get(label).copied().unwrap_or(0)
    }

    fn render(&self, name: &str, help: &str, key: &str, out: &mut String) {
        out.push_str(&format!(
            "# HELP {name} {help}
# TYPE {name} counter
"
        ));
        for (label, n) in self.values.lock().iter() {
            out.push_str(&format!(
                "{name}{{{key}=\"{label}\"}} {n}
"
            ));
        }
    }
}

#[derive(Default)]
pub struct Metrics {
    pub calls_active: AtomicU64,
    pub calls_total: AtomicU64,
    pub turns_total: AtomicU64,
    pub barge_ins_total: AtomicU64,
    pub handoffs_total: AtomicU64,
    pub llm_calls_total: AtomicU64,
    pub llm_failures_total: AtomicU64,
    pub action_failures_total: AtomicU64,
    /// Caller stopped speaking → first reply frame sent. The product latency target.
    pub response_latency: Histogram,
    /// Caller started speaking over the agent → playout cancelled.
    pub barge_in_latency: Histogram,
    pub llm_latency: Histogram,
    /// End of the caller's speech (finalize sent) → final transcript.
    pub stt_final: Histogram,
    pub tts_first_chunk: Histogram,
    pub action_latency: Histogram,
    /// Caller stopped speaking → the agent's first decision (token or phrase).
    pub agent_first: Histogram,
    /// Silence the VAD waited after the caller's last sound before ending the utterance.
    pub vad_endpoint: Histogram,
    /// How long the caller's utterances were.
    pub caller_speech: Histogram,
    /// Silence the caller heard between two pieces of one reply.
    pub audio_gap: Histogram,
    /// Agent audio still to play when the caller interrupted it.
    pub barge_in_remaining: Histogram,
    /// Interruptions by what stopped the agent (`words`, `voiced_duration`, ...).
    pub barge_in_reasons: LabeledCounter,
    /// Voice over the agent that did not stop it (`noise`, `backchannel`, `short`).
    pub barge_in_suppressed: LabeledCounter,
    /// Gaps over the warning threshold (`seam` between items, `underrun` inside one).
    pub audio_gaps: LabeledCounter,
    /// Calls by what went wrong in them (`booking_left_unfinished`, `same_question_twice`,
    /// `caller_hung_up_mid_booking`, `slow_turns`, ...).
    pub call_problems: LabeledCounter,
    /// Interruptions that let the agent finish a nearly finished sentence.
    pub sentence_end_protected_total: AtomicU64,
    /// Audio segments by source: cached / template / tts.
    segments: Mutex<BTreeMap<&'static str, u64>>,
}

impl Metrics {
    pub fn segment(&self, source: &'static str) {
        *self.segments.lock().entry(source).or_default() += 1;
    }

    pub fn segments(&self) -> BTreeMap<&'static str, u64> {
        self.segments.lock().clone()
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        let g = |o: &mut String, name: &str, help: &str, kind: &str, v: &AtomicU64| {
            o.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {}\n", v.load(Ordering::Relaxed)));
        };
        g(&mut out, "callora_calls_active", "Calls in progress.", "gauge", &self.calls_active);
        g(&mut out, "callora_calls_total", "Calls handled.", "counter", &self.calls_total);
        g(&mut out, "callora_turns_total", "Caller turns understood.", "counter", &self.turns_total);
        g(
            &mut out,
            "callora_barge_ins_total",
            "Times the caller interrupted the agent.",
            "counter",
            &self.barge_ins_total,
        );
        g(&mut out, "callora_handoffs_total", "Calls transferred to a human.", "counter", &self.handoffs_total);
        g(&mut out, "callora_llm_calls_total", "LLM understanding requests.", "counter", &self.llm_calls_total);
        g(
            &mut out,
            "callora_llm_failures_total",
            "LLM requests that failed or timed out.",
            "counter",
            &self.llm_failures_total,
        );
        g(
            &mut out,
            "callora_action_failures_total",
            "Business actions that failed.",
            "counter",
            &self.action_failures_total,
        );
        out.push_str("# HELP callora_audio_segments_total Reply audio segments by source.\n# TYPE callora_audio_segments_total counter\n");
        for (source, n) in self.segments() {
            out.push_str(&format!("callora_audio_segments_total{{source=\"{source}\"}} {n}\n"));
        }
        self.response_latency.render(
            "callora_response_latency_ms",
            "Caller speech end to first reply audio.",
            "",
            &mut out,
        );
        self.barge_in_latency.render(
            "callora_barge_in_latency_ms",
            "Caller speech start to agent audio cancelled.",
            "",
            &mut out,
        );
        self.llm_latency.render("callora_llm_latency_ms", "LLM understanding latency.", "", &mut out);
        self.stt_final.render("callora_stt_final_ms", "End of speech to final transcript.", "", &mut out);
        self.tts_first_chunk.render("callora_tts_first_chunk_ms", "Dynamic TTS time to first audio.", "", &mut out);
        self.action_latency.render("callora_action_latency_ms", "Business action latency.", "", &mut out);
        self.agent_first.render(
            "callora_agent_first_ms",
            "Caller speech end to the agent's first decision.",
            "",
            &mut out,
        );
        self.vad_endpoint.render("callora_vad_endpoint_ms", "Silence waited to end an utterance.", "", &mut out);
        self.caller_speech.render("callora_caller_speech_ms", "Length of the caller's utterances.", "", &mut out);
        self.audio_gap.render("callora_audio_gap_ms", "Silence between audio of one reply.", "", &mut out);
        self.barge_in_remaining.render(
            "callora_barge_in_remaining_ms",
            "Agent audio left to play when interrupted.",
            "",
            &mut out,
        );
        self.barge_in_reasons.render(
            "callora_barge_in_reason_total",
            "Interruptions by what stopped the agent.",
            "reason",
            &mut out,
        );
        self.barge_in_suppressed.render(
            "callora_barge_in_suppressed_total",
            "Voice over the agent that did not stop it.",
            "reason",
            &mut out,
        );
        self.audio_gaps.render("callora_audio_gaps_total", "Audio gaps over the warning threshold.", "kind", &mut out);
        self.call_problems.render(
            "callora_call_problems_total",
            "Calls by what went wrong in them.",
            "problem",
            &mut out,
        );
        g(
            &mut out,
            "callora_sentence_end_protected_total",
            "Interruptions that let a nearly finished sentence end.",
            "counter",
            &self.sentence_end_protected_total,
        );
        out
    }
}

//! Deciding whether the caller's voice over the agent is an interruption.
//!
//! The VAD only says "someone is making sound". A cough, a car horn, a TV, or the caller
//! saying "כן" / "אהה" while the agent talks are not reasons to stop the agent mid-sentence.
//! This module is the one place that decides, from what is known so far about the voice
//! (how long, how loud) and what the recognizer has heard of it (how many real words), and
//! says *why* it stopped the agent (the reason is a metric and a log field).
//!
//! It is pure: no clocks, no audio. The session feeds it and acts on the answer.
//!
//! The rules, in order:
//! 1. A voice that goes on long enough is an interruption whatever it says
//!    ([`BargeReason::Voiced`]): a caller who talks for a second is not noise.
//! 2. Words that are not just listening sounds stop the agent once the voice has lasted
//!    `min_voiced_ms` (so a half-second of TV words is not enough) and there are
//!    `min_words` of them, or one word once the voice lasted `single_word_ms`
//!    ([`BargeReason::Words`]).
//! 3. A loud voice, well above the line's threshold, that goes on for `strong_ms` is the
//!    caller talking over the agent, words or not ([`BargeReason::Loud`]). Never over the
//!    greeting: callers start talking ("הלו", "כן?") as the line opens, and a live call lost
//!    its greeting in its first half second.
//!
//! Listening sounds alone ("כן", "אהה", "mm") only count under rule 1.
//!
//! `BargeConfig::legacy()` is the behaviour before this module existed: any words, at once.

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BargeConfig {
    /// A voice this long stops the agent, words or not.
    pub confirm_ms: u64,
    /// The same over the greeting, when callers say "הלו", "כן?" as the line opens.
    pub confirm_greeting_ms: u64,
    /// The same during a read-back, where "כן", "אהה" are the caller listening.
    pub confirm_read_back_ms: u64,
    /// Voice needed before words count.
    pub min_voiced_ms: u64,
    /// Words needed (listening sounds and noise are not words).
    pub min_words: usize,
    /// Voice needed before a single word counts.
    pub single_word_ms: u64,
    /// A loud voice this long stops the agent. 0 turns the rule off.
    pub strong_ms: u64,
    /// "Loud": the utterance's mean level over the VAD threshold...
    pub strong_rms_ratio: f32,
    /// ...and never under this level (RMS, 16-bit scale). The VAD threshold went down from
    /// 900 to 600 to hear quiet callers; without this floor "loud" went down with it, and a
    /// normal voice (1800) cut the greeting.
    pub strong_min_rms: f32,
    /// A final transcript that arrives while the agent talks stops it only when it has real
    /// words and the caller's voice lasted this long; shorter is background speech or a
    /// late result for a blip; it is then still answered, after the agent finishes. `0`: any
    /// final stops the agent (the old behaviour).
    pub final_min_voiced_ms: u64,
}

impl Default for BargeConfig {
    fn default() -> Self {
        Self {
            confirm_ms: 900,
            confirm_greeting_ms: 1500,
            confirm_read_back_ms: 1200,
            min_voiced_ms: 300,
            min_words: 2,
            single_word_ms: 500,
            strong_ms: 500,
            strong_rms_ratio: 2.5,
            strong_min_rms: 2250.0,
            final_min_voiced_ms: 200,
        }
    }
}

impl BargeConfig {
    /// Words stop the agent the moment the recognizer returns them, as before.
    pub fn legacy() -> Self {
        Self {
            min_voiced_ms: 0,
            min_words: 1,
            single_word_ms: 0,
            strong_ms: 0,
            final_min_voiced_ms: 0,
            ..Self::default()
        }
    }

    pub fn confirm_for(&self, phase: Phase) -> Duration {
        Duration::from_millis(match phase {
            Phase::Normal => self.confirm_ms,
            Phase::Greeting => self.confirm_greeting_ms,
            Phase::ReadBack => self.confirm_read_back_ms,
        })
    }
}

/// What the agent is saying when the caller's voice begins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Normal,
    Greeting,
    ReadBack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BargeReason {
    /// Real words, over enough voice.
    Words,
    /// A voice that went on.
    Voiced,
    /// A loud voice that went on.
    Loud,
    /// A final transcript arrived while the agent was still talking.
    Final,
    /// A "yes" over a read-back the caller had heard to its end.
    Answer,
}

impl BargeReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Words => "words",
            Self::Voiced => "voiced_duration",
            Self::Loud => "loud_sustained",
            Self::Final => "final_transcript",
            Self::Answer => "yes_over_readback",
        }
    }
}

/// What is known about the voice so far.
#[derive(Debug, Clone, Copy)]
pub struct BargeInput {
    pub voiced_ms: u64,
    /// Words the recognizer heard that are not listening sounds, noise or a "hello".
    pub real_words: usize,
    /// Everything heard so far is a listening sound ("כן", "אהה").
    pub listening_only: bool,
    pub mean_rms: f32,
    pub threshold: f32,
    pub phase: Phase,
}

pub fn classify(cfg: &BargeConfig, i: &BargeInput) -> Option<BargeReason> {
    if i.voiced_ms >= cfg.confirm_for(i.phase).as_millis() as u64 {
        return Some(BargeReason::Voiced);
    }
    if i.listening_only {
        return None;
    }
    if i.real_words >= cfg.min_words.max(1) && i.voiced_ms >= cfg.min_voiced_ms {
        return Some(BargeReason::Words);
    }
    if i.real_words >= 1 && i.voiced_ms >= cfg.single_word_ms {
        return Some(BargeReason::Words);
    }
    let loud = i.mean_rms >= (i.threshold * cfg.strong_rms_ratio).max(cfg.strong_min_rms);
    if cfg.strong_ms > 0 && i.phase != Phase::Greeting && i.voiced_ms >= cfg.strong_ms && loud {
        return Some(BargeReason::Loud);
    }
    None
}

/// A final transcript arrived while the agent was talking: does it stop the agent? The same
/// rules as [`classify`], except that the transcript itself is the evidence of words, so
/// real words over `final_min_voiced_ms` of voice are enough. `None`: background speech or a
/// blip; the agent goes on (the transcript is still answered, when it has finished).
pub fn classify_final(cfg: &BargeConfig, i: &BargeInput) -> Option<BargeReason> {
    if let Some(reason) = classify(cfg, i) {
        return Some(reason);
    }
    (!i.listening_only && i.real_words >= 1 && i.voiced_ms >= cfg.final_min_voiced_ms).then_some(BargeReason::Final)
}

/// Why a voice that never became an interruption did not: for the metric.
pub fn suppressed_label(listening_only: bool, real_words: usize) -> &'static str {
    if listening_only {
        "backchannel"
    } else if real_words == 0 {
        "noise"
    } else {
        "short"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(voiced_ms: u64, real_words: usize, listening_only: bool, mean_rms: f32) -> BargeInput {
        BargeInput { voiced_ms, real_words, listening_only, mean_rms, threshold: 900.0, phase: Phase::Normal }
    }

    #[test]
    fn short_noise_without_words_never_interrupts() {
        let cfg = BargeConfig::default();
        for ms in [100, 300, 600, 890] {
            assert_eq!(classify(&cfg, &input(ms, 0, false, 1200.0)), None, "{ms} ms of quiet noise");
        }
    }

    #[test]
    fn loud_noise_needs_to_go_on_before_it_interrupts() {
        let cfg = BargeConfig::default();
        assert_eq!(classify(&cfg, &input(400, 0, false, 5000.0)), None, "a loud burst is not enough");
        assert_eq!(classify(&cfg, &input(500, 0, false, 5000.0)), Some(BargeReason::Loud));
    }

    #[test]
    fn backchannels_only_interrupt_by_going_on_for_long() {
        let cfg = BargeConfig::default();
        // "כן", "אהה": short, however loud.
        assert_eq!(classify(&cfg, &input(450, 0, true, 6000.0)), None);
        assert_eq!(classify(&cfg, &input(850, 0, true, 6000.0)), None);
        assert_eq!(classify(&cfg, &input(900, 0, true, 6000.0)), Some(BargeReason::Voiced));
        // Over a read-back they get more time.
        let mut i = input(900, 0, true, 6000.0);
        i.phase = Phase::ReadBack;
        assert_eq!(classify(&cfg, &i), None);
        i.voiced_ms = 1200;
        assert_eq!(classify(&cfg, &i), Some(BargeReason::Voiced));
        // And over the greeting more still.
        i.phase = Phase::Greeting;
        i.voiced_ms = 1400;
        assert_eq!(classify(&cfg, &i), None);
    }

    #[test]
    fn real_words_interrupt_quickly_but_not_instantly() {
        let cfg = BargeConfig::default();
        assert_eq!(classify(&cfg, &input(150, 3, false, 1500.0)), None, "background words in a blink");
        assert_eq!(classify(&cfg, &input(300, 2, false, 1500.0)), Some(BargeReason::Words));
        assert_eq!(classify(&cfg, &input(300, 1, false, 1500.0)), None, "one word needs longer");
        assert_eq!(classify(&cfg, &input(500, 1, false, 1500.0)), Some(BargeReason::Words));
    }

    #[test]
    fn a_clear_interruption_lands_within_half_a_second_even_before_words() {
        // "לא, רגע, תעצור" at a normal speaking level over a quiet line.
        let cfg = BargeConfig::default();
        assert_eq!(classify(&cfg, &input(500, 0, false, 2400.0)), Some(BargeReason::Loud));
    }

    #[test]
    fn legacy_is_words_at_once() {
        let cfg = BargeConfig::legacy();
        assert_eq!(classify(&cfg, &input(100, 1, false, 1000.0)), Some(BargeReason::Words));
        assert_eq!(classify(&cfg, &input(600, 0, false, 9000.0)), None, "no loud rule");
        assert_eq!(classify(&cfg, &input(900, 0, false, 1000.0)), Some(BargeReason::Voiced));
    }

    #[test]
    fn a_final_stops_the_agent_only_with_words_and_some_voice() {
        let cfg = BargeConfig::default();
        let at = |voiced, words, listening| classify_final(&cfg, &input(voiced, words, listening, 1500.0));
        assert_eq!(at(120, 3, false), None, "a blip of voice with words: background speech");
        assert_eq!(at(250, 1, false), Some(BargeReason::Final), "a real one-word answer");
        assert_eq!(at(600, 0, false), None, "no words");
        assert_eq!(at(400, 0, true), None, "only a listening sound");
        assert_eq!(at(950, 0, true), Some(BargeReason::Voiced), "a long voice still counts");
        assert_eq!(classify_final(&BargeConfig::legacy(), &input(0, 1, false, 0.0)), Some(BargeReason::Words));
    }

    #[test]
    fn suppression_labels() {
        assert_eq!(suppressed_label(true, 0), "backchannel");
        assert_eq!(suppressed_label(false, 0), "noise");
        assert_eq!(suppressed_label(false, 2), "short");
    }

    #[test]
    fn loud_is_measured_against_the_old_level_and_never_cuts_the_greeting() {
        let cfg = BargeConfig::default();
        // The live call: a normal voice at 1800 over a 600 threshold is not "loud".
        let mut i = BargeInput {
            voiced_ms: 500,
            real_words: 0,
            listening_only: false,
            mean_rms: 1800.0,
            threshold: 600.0,
            phase: Phase::Normal,
        };
        assert_eq!(classify(&cfg, &i), None);
        i.mean_rms = 3000.0;
        assert_eq!(classify(&cfg, &i), Some(BargeReason::Loud));
        // Over the greeting only words or a long voice stop it.
        i.phase = Phase::Greeting;
        assert_eq!(classify(&cfg, &i), None);
        i.voiced_ms = 1500;
        assert_eq!(classify(&cfg, &i), Some(BargeReason::Voiced));
    }
}

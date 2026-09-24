//! Energy-based voice activity on the caller's inbound track.
//!
//! Twilio's inbound track carries only the caller's microphone, so sustained energy on it
//! while the agent talks is the caller talking over it. Two decisions come from here:
//! - **barge-in** (speech started): stop the agent within ~100 ms, long before any STT
//!   transcript could say so;
//! - **endpoint** (speech ended): ask the STT to finalize now instead of waiting for its
//!   own, longer silence timeout. This is most of the speech-end → reply latency.
//!
//! Tuned from the legacy system: -31 dBFS threshold, and quiet frames drain the speech
//! counter twice as fast as speech fills it, so clicks and breaths never add up.

use crate::mulaw::rms;

#[derive(Debug, Clone, Copy)]
pub struct VadConfig {
    pub threshold_rms: f32,
    /// Speech needed before it counts as the caller talking (barge-in trigger).
    pub trigger_ms: u64,
    /// Silence after speech that ends the utterance.
    pub endpoint_ms: u64,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self { threshold_rms: 900.0, trigger_ms: 100, endpoint_ms: 400 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadEvent {
    SpeechStarted,
    SpeechEnded,
}

#[derive(Debug)]
pub struct Vad {
    cfg: VadConfig,
    speech_ms: u64,
    silence_ms: u64,
    speaking: bool,
    pub last_rms: f32,
}

impl Vad {
    pub fn new(cfg: VadConfig) -> Self {
        Self { cfg, speech_ms: 0, silence_ms: 0, speaking: false, last_rms: 0.0 }
    }

    pub fn is_speaking(&self) -> bool {
        self.speaking
    }

    /// Feed one inbound μ-law frame.
    pub fn push(&mut self, frame: &[u8]) -> Option<VadEvent> {
        let frame_ms = (frame.len() as u64 * 1000) / 8000;
        self.last_rms = rms(frame);
        if self.last_rms >= self.cfg.threshold_rms {
            self.speech_ms = (self.speech_ms + frame_ms).min(2 * self.cfg.trigger_ms);
            self.silence_ms = 0;
        } else {
            self.speech_ms = self.speech_ms.saturating_sub(2 * frame_ms);
            self.silence_ms += frame_ms;
        }
        if !self.speaking && self.speech_ms >= self.cfg.trigger_ms {
            self.speaking = true;
            return Some(VadEvent::SpeechStarted);
        }
        if self.speaking && self.silence_ms >= self.cfg.endpoint_ms {
            self.speaking = false;
            self.speech_ms = 0;
            return Some(VadEvent::SpeechEnded);
        }
        None
    }

    pub fn reset(&mut self) {
        self.speech_ms = 0;
        self.silence_ms = 0;
        self.speaking = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mulaw::{encode, FRAME_BYTES, SILENCE};

    fn loud() -> Vec<u8> {
        (0..FRAME_BYTES).map(|i| encode(if i % 2 == 0 { 4000 } else { -4000 })).collect()
    }

    #[test]
    fn detects_start_quickly_and_end_after_silence() {
        let mut vad = Vad::new(VadConfig::default());
        let quiet = vec![SILENCE; FRAME_BYTES];
        assert_eq!(vad.push(&quiet), None);
        let mut started_after = None;
        for n in 1..=10 {
            if vad.push(&loud()) == Some(VadEvent::SpeechStarted) {
                started_after = Some(n * 20);
                break;
            }
        }
        assert_eq!(started_after, Some(100), "barge-in after 100 ms of speech");
        let mut ended_after = None;
        for n in 1..=40 {
            if vad.push(&quiet) == Some(VadEvent::SpeechEnded) {
                ended_after = Some(n * 20);
                break;
            }
        }
        assert_eq!(ended_after, Some(400));
    }

    #[test]
    fn a_click_is_not_speech() {
        let mut vad = Vad::new(VadConfig::default());
        let quiet = vec![SILENCE; FRAME_BYTES];
        for _ in 0..20 {
            vad.push(&loud());
            vad.push(&quiet);
            vad.push(&quiet);
        }
        assert!(!vad.is_speaking());
    }
}

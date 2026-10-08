//! Energy-based voice activity on the caller's inbound track.
//!
//! Twilio's inbound track carries only the caller's microphone, so sustained energy on it
//! while the agent talks is the caller talking over it. Two decisions come from here:
//! - **barge-in** (speech started): stop the agent within ~100 ms, long before any STT
//!   transcript could say so;
//! - **endpoint** (speech ended): ask the STT to finalize now instead of waiting for its
//!   own, longer silence timeout. This is most of the speech-end → reply latency.
//!
//! Tuned from the legacy system: -31 dBFS threshold (lowered to 600, -35 dBFS, after live calls
//! showed quiet callers whose words sat at 600-900 RMS: the end of their sentences counted as
//! silence, the utterance was cut early and the rest became a second, lost, transcript), and quiet frames drain the speech
//! counter twice as fast as speech fills it, so clicks and breaths never add up.

use crate::mulaw::rms;

#[derive(Debug, Clone, Copy)]
pub struct VadConfig {
    pub threshold_rms: f32,
    /// Speech needed before it counts as the caller talking (barge-in trigger).
    pub trigger_ms: u64,
    /// Silence after speech that ends the utterance.
    pub endpoint_ms: u64,
    /// Longer "speech" than this is steady noise (a car, wind, a TV): it ends, and the noise
    /// level becomes the threshold. Without it the call waited for the end of a sentence
    /// that never came.
    pub max_speech_ms: u64,
    /// Speech must be this many times louder than the background noise (followed as the call
    /// goes): at a bus stop the noise itself is above the fixed threshold, and the end of a
    /// sentence was heard 2 s late. 0 is off.
    pub noise_floor_ratio: f32,
    /// With a voice probability for each frame (RNNoise), how sure it must be that a voice
    /// starts speech; a bus braking is loud but no voice. `None` is the energy alone.
    pub voice_start: Option<f32>,
    /// And how sure it must stay while speech goes on (lower, so a soft syllable does not
    /// end a sentence).
    pub voice_hold: f32,
}

impl VadConfig {
    /// The caller in a noisy place: background-following threshold and the voice
    /// probability. On 110 noisy clips (bus station, traffic, wind, a cafe, a TV, a market)
    /// the end of speech was heard 0.3 s after it instead of 2 s, and noise alone started
    /// "speech" in 7 of 21 clips instead of 16.
    pub fn for_noise(self) -> Self {
        Self { noise_floor_ratio: 2.0, voice_start: Some(0.9), voice_hold: 0.3, ..self }
    }
}

impl Default for VadConfig {
    fn default() -> Self {
        // 400 ms cut live Hebrew calls mid-word with Cartesia, whose finalize dropped the rest
        // of the sentence. Scribe keeps every word (a word after an early commit starts the
        // next segment, and a new sentence while the agent thinks joins the utterance), so
        // 500 ms is safe and saves 200 ms on every turn.
        Self {
            threshold_rms: 600.0,
            trigger_ms: 100,
            endpoint_ms: 500,
            max_speech_ms: 15_000,
            noise_floor_ratio: 0.0,
            voice_start: None,
            voice_hold: 0.5,
        }
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
    /// The threshold now: the configured one, or above the steady noise heard.
    threshold: f32,
    speaking_ms: u64,
    loudness: f64,
    peak: f32,
    /// Silence that ends the utterance now (the configured one unless the runtime moved it).
    endpoint_ms: u64,
    /// The background level, followed down fast and up slowly.
    floor: f32,
}

impl Vad {
    pub fn new(cfg: VadConfig) -> Self {
        Self {
            cfg,
            speech_ms: 0,
            silence_ms: 0,
            speaking: false,
            last_rms: 0.0,
            threshold: cfg.threshold_rms,
            speaking_ms: 0,
            loudness: 0.0,
            peak: 0.0,
            endpoint_ms: cfg.endpoint_ms,
            floor: 300.0,
        }
    }

    pub fn is_speaking(&self) -> bool {
        self.speaking
    }

    /// Move the silence that ends an utterance (the runtime shortens it when the caller has
    /// plainly finished a short answer, and lengthens it when the sentence is unfinished).
    pub fn set_endpoint_ms(&mut self, ms: u64) {
        self.endpoint_ms = ms.max(60);
    }

    /// The silence that ended (or would end) the current utterance, in ms.
    pub fn silence_ms(&self) -> u64 {
        self.silence_ms
    }

    /// The utterance so far (or the last one): how long it was, and how loud (RMS, 16-bit
    /// scale; the mean over its frames and the loudest frame).
    pub fn speaking_ms(&self) -> u64 {
        self.speaking_ms
    }

    pub fn mean_rms(&self) -> f32 {
        if self.speaking_ms == 0 {
            0.0
        } else {
            (self.loudness / self.speaking_ms as f64) as f32
        }
    }

    pub fn peak_rms(&self) -> f32 {
        self.peak
    }

    /// The level that counts as speech now: the threshold, or above the background noise.
    pub fn threshold(&self) -> f32 {
        if self.cfg.noise_floor_ratio > 0.0 {
            self.threshold.max(self.floor * self.cfg.noise_floor_ratio)
        } else {
            self.threshold
        }
    }

    /// The background noise level heard (RMS).
    pub fn noise_floor(&self) -> f32 {
        self.floor
    }

    /// Feed one inbound μ-law frame.
    pub fn push(&mut self, frame: &[u8]) -> Option<VadEvent> {
        self.push_with(frame, None)
    }

    /// Feed one inbound μ-law frame with how likely it holds a voice (0 to 1), when known.
    pub fn push_with(&mut self, frame: &[u8], voice: Option<f32>) -> Option<VadEvent> {
        let frame_ms = (frame.len() as u64 * 1000) / 8000;
        self.last_rms = rms(frame);
        let threshold = self.threshold();
        self.floor = if self.last_rms < self.floor {
            0.9 * self.floor + 0.1 * self.last_rms
        } else {
            0.995 * self.floor + 0.005 * self.last_rms
        };
        let sure = if self.speaking { Some(self.cfg.voice_hold) } else { self.cfg.voice_start };
        let voiced = match (self.cfg.voice_start, voice) {
            (Some(_), Some(p)) => p >= sure.unwrap_or(0.0),
            _ => true,
        };
        if self.last_rms >= threshold && voiced {
            self.speech_ms = (self.speech_ms + frame_ms).min(2 * self.cfg.trigger_ms);
            self.silence_ms = 0;
        } else {
            self.speech_ms = self.speech_ms.saturating_sub(2 * frame_ms);
            self.silence_ms += frame_ms;
        }
        if !self.speaking && self.speech_ms >= self.cfg.trigger_ms {
            self.speaking = true;
            self.speaking_ms = 0;
            self.loudness = 0.0;
            self.peak = 0.0;
            return Some(VadEvent::SpeechStarted);
        }
        if self.speaking {
            self.speaking_ms += frame_ms;
            self.loudness += f64::from(self.last_rms) * frame_ms as f64;
            self.peak = self.peak.max(self.last_rms);
            if self.speaking_ms >= self.cfg.max_speech_ms {
                let mean = (self.loudness / self.speaking_ms as f64) as f32;
                self.threshold = self.threshold.max(mean * 1.2);
                tracing::info!(threshold = self.threshold, "steady noise, not speech: the threshold goes above it");
                self.speaking = false;
                self.speech_ms = 0;
                self.silence_ms = 0;
                return Some(VadEvent::SpeechEnded);
            }
        }
        if self.speaking && self.silence_ms >= self.endpoint_ms {
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
        assert_eq!(ended_after, Some(500));
    }

    #[test]
    fn the_endpoint_can_be_moved_and_the_utterance_is_measured() {
        let mut vad = Vad::new(VadConfig::default());
        let quiet = vec![SILENCE; FRAME_BYTES];
        vad.set_endpoint_ms(300);
        for _ in 0..10 {
            vad.push(&loud());
        }
        assert!(vad.speaking_ms() >= 100 && vad.mean_rms() > 3000.0 && vad.peak_rms() >= vad.mean_rms());
        let ended = (1..=40).find(|_| vad.push(&quiet) == Some(VadEvent::SpeechEnded)).map(|n| n * 20);
        assert!(ended.is_some(), "ends");
        let mut vad = Vad::new(VadConfig::default());
        vad.set_endpoint_ms(300);
        for _ in 0..10 {
            vad.push(&loud());
        }
        let n = (1..=40).find(|_| vad.push(&quiet) == Some(VadEvent::SpeechEnded)).unwrap();
        assert_eq!(vad.silence_ms(), 300, "after {n} frames");
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

    #[test]
    fn steady_noise_ends_and_raises_the_threshold() {
        let mut vad = Vad::new(VadConfig::default());
        let mut events = Vec::new();
        for _ in 0..(16_000 / 20) {
            if let Some(e) = vad.push(&loud()) {
                events.push(e);
            }
        }
        assert_eq!(events, vec![VadEvent::SpeechStarted, VadEvent::SpeechEnded], "it ends after 15 s");
        // The same noise again is not speech any more.
        assert!((0..100).all(|_| vad.push(&loud()).is_none()) && !vad.is_speaking());
    }
}

#[cfg(test)]
mod noise_tests {
    use super::*;
    use crate::mulaw::{encode, FRAME_BYTES};

    fn level(amp: i16) -> Vec<u8> {
        (0..FRAME_BYTES).map(|i| encode(if i % 2 == 0 { amp } else { -amp })).collect()
    }

    #[test]
    fn steady_background_raises_the_level_that_counts_as_speech() {
        let mut vad = Vad::new(VadConfig::default().for_noise());
        for _ in 0..300 {
            vad.push_with(&level(900), Some(0.0));
        }
        assert!(vad.noise_floor() > 700.0, "{}", vad.noise_floor());
        assert!(vad.threshold() > 1400.0, "{}", vad.threshold());
        assert!(!vad.is_speaking(), "a loud background with no voice is no speech");
        // A voice twice as loud as the background is speech.
        let started = (0..20).any(|_| vad.push_with(&level(4000), Some(0.95)) == Some(VadEvent::SpeechStarted));
        assert!(started);
        // Back to the background alone: the sentence ends after the endpoint, not seconds later.
        let ended = (1..=40).find(|_| vad.push_with(&level(900), Some(0.1)) == Some(VadEvent::SpeechEnded));
        assert_eq!(ended, Some(25), "500 ms");
    }

    #[test]
    fn loud_sound_with_no_voice_does_not_start_speech_but_a_soft_syllable_keeps_it() {
        let mut vad = Vad::new(VadConfig::default().for_noise());
        assert!((0..50).all(|_| vad.push_with(&level(5000), Some(0.2)).is_none()), "a horn is no speech");
        let mut vad = Vad::new(VadConfig::default().for_noise());
        assert!((0..10).any(|_| vad.push_with(&level(4000), Some(0.95)) == Some(VadEvent::SpeechStarted)));
        assert!((0..40).all(|_| vad.push_with(&level(4000), Some(0.6)).is_none()), "less sure, still speech");
        assert!(vad.is_speaking());
    }

    #[test]
    fn without_a_voice_probability_it_is_the_energy_vad() {
        let mut vad = Vad::new(VadConfig::default().for_noise());
        assert!((0..10).any(|_| vad.push(&level(4000)) == Some(VadEvent::SpeechStarted)));
    }
}

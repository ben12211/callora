//! Noise suppression and voice probability for the caller's track, with RNNoise (a small
//! recurrent network, run here in pure Rust by `nnnoiseless`, about 0.1 ms per 20 ms frame).
//!
//! The energy VAD counts anything loud as speech: a bus braking, wind on the microphone, a
//! horn. RNNoise says, every 10 ms, how likely the sound is a human voice, and gives the frame
//! back with the steady noise taken out. It works at 48 kHz: the call's 8 kHz is brought up
//! six times (linear steps between samples) and back down by averaging each six.

use nnnoiseless::DenoiseState;

use crate::mulaw;

/// Samples of one RNNoise frame (10 ms at 48 kHz).
const FRAME: usize = DenoiseState::FRAME_SIZE;
const UP: usize = 6;

pub struct Denoiser {
    state: Box<DenoiseState<'static>>,
    /// The last 8 kHz sample of the previous frame, for the steps into this one.
    last: f32,
    /// 48 kHz samples not yet a whole RNNoise frame.
    pending: Vec<f32>,
    /// Denoised 48 kHz samples not yet brought down.
    ready: Vec<f32>,
    /// The voice probability of the frames processed since the last call.
    probs: Vec<f32>,
    /// RNNoise's first frame is its warm-up: its output is dropped.
    warmed: bool,
}

/// One frame of the caller's audio, cleaned, and how likely it held a voice (0 to 1).
pub struct Cleaned {
    pub audio: Vec<u8>,
    pub voice: f32,
}

impl Default for Denoiser {
    fn default() -> Self {
        Self::new()
    }
}

impl Denoiser {
    pub fn new() -> Self {
        Self {
            state: DenoiseState::new(),
            last: 0.0,
            pending: Vec::with_capacity(2 * FRAME),
            ready: Vec::with_capacity(2 * FRAME),
            probs: Vec::new(),
            warmed: false,
        }
    }

    /// Feed one μ-law frame (any length; 160 bytes is 20 ms). The cleaned audio has the same
    /// length, delayed by RNNoise's 10 ms frame; the probability is the highest of the
    /// RNNoise frames finished during it (0 until the first one).
    pub fn push(&mut self, frame: &[u8]) -> Cleaned {
        for &b in frame {
            let s = f32::from(mulaw::decode(b));
            for step in 1..=UP {
                self.pending.push(self.last + (s - self.last) * step as f32 / UP as f32);
            }
            self.last = s;
        }
        let mut out = [0.0f32; FRAME];
        while self.pending.len() >= FRAME {
            let input: Vec<f32> = self.pending.drain(..FRAME).collect();
            let p = self.state.process_frame(&mut out, &input);
            if self.warmed {
                self.ready.extend_from_slice(&out);
            } else {
                self.warmed = true;
                self.ready.extend(std::iter::repeat_n(0.0, FRAME));
            }
            self.probs.push(p);
        }
        let n = frame.len().min(self.ready.len() / UP);
        let mut audio: Vec<u8> = self
            .ready
            .drain(..n * UP)
            .collect::<Vec<_>>()
            .chunks(UP)
            .map(|c| mulaw::encode((c.iter().sum::<f32>() / UP as f32).clamp(-32768.0, 32767.0) as i16))
            .collect();
        // The first frame's audio is not out yet: silence keeps the length.
        audio.resize(frame.len(), mulaw::SILENCE);
        let voice = self.probs.drain(..).fold(0.0f32, f32::max);
        Cleaned { audio, voice }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mulaw::{encode, rms, FRAME_BYTES};

    fn tone(hz: f32, amp: f32, frames: usize) -> Vec<Vec<u8>> {
        (0..frames)
            .map(|f| {
                (0..FRAME_BYTES)
                    .map(|i| {
                        let t = (f * FRAME_BYTES + i) as f32 / 8000.0;
                        encode((amp * (2.0 * std::f32::consts::PI * hz * t).sin()) as i16)
                    })
                    .collect()
            })
            .collect()
    }

    fn hiss(amp: f32, frames: usize) -> Vec<Vec<u8>> {
        let mut x: u32 = 12345;
        (0..frames)
            .map(|_| {
                (0..FRAME_BYTES)
                    .map(|_| {
                        x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
                        encode(((x >> 16) as f32 / 32768.0 - 1.0).mul_add(amp, 0.0) as i16)
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn every_frame_comes_back_the_same_length() {
        let mut d = Denoiser::new();
        for f in tone(300.0, 3000.0, 20) {
            assert_eq!(d.push(&f).audio.len(), FRAME_BYTES);
        }
        assert_eq!(d.push(&[0xFF; 37]).audio.len(), 37);
    }

    #[test]
    fn steady_hiss_is_taken_down() {
        let mut d = Denoiser::new();
        let frames = hiss(2000.0, 200);
        let (mut before, mut after) = (0.0, 0.0);
        for (i, f) in frames.iter().enumerate() {
            let c = d.push(f);
            if i >= 100 {
                before += rms(f);
                after += rms(&c.audio);
            }
        }
        // Its voice probability is not tested: band-limited hiss scores high, and what the VAD
        // makes of it was measured on recorded street noise instead.
        assert!(after < before / 2.0, "the hiss is suppressed: {before} -> {after}");
    }
}

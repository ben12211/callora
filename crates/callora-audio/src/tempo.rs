//! Faster speech at the same pitch (WSOLA), on μ-law 8 kHz audio.
//!
//! eleven_v4_turbo, the voice the owner chose for its clarity, speaks about 40% slower than
//! eleven_v3 and ignores the `speed` setting ("נורא איטי, נמרח"). The audio is sped up here
//! instead: overlapping windows of the speech are taken a little further apart than they
//! are put back, each one shifted (within a few milliseconds) to where it continues the
//! last one best, so words get shorter and the voice stays the voice.
//!
//! [`Tempo`] works on a stream as it arrives (live speech is played while it is still being
//! synthesized); [`stretch`] is the same on a whole clip.

use crate::mulaw::{decode, encode};

/// Window length: 32 ms at 8 kHz.
const N: usize = 256;
/// Output hop: windows overlap by half.
const HS: usize = N / 2;
/// How far a window may move to continue the last one best: 8 ms either way.
const SEARCH: usize = 64;

pub struct Tempo {
    tempo: f64,
    input: Vec<f32>,
    output: Vec<f32>,
    emitted: usize,
    frame: usize,
    previous: Option<usize>,
    window: Vec<f32>,
}

impl Tempo {
    /// `tempo` 1.25 makes speech 25% faster; 1.0 leaves it as it is.
    pub fn new(tempo: f32) -> Self {
        let window = (0..N).map(|n| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * n as f32 / N as f32).cos()).collect();
        Self {
            tempo: f64::from(tempo.clamp(0.5, 2.0)),
            input: Vec::new(),
            output: Vec::new(),
            emitted: 0,
            frame: 0,
            previous: None,
            window,
        }
    }

    fn analysis_hop(&self) -> f64 {
        HS as f64 * self.tempo
    }

    /// μ-law in; the μ-law that is final so far out.
    pub fn push(&mut self, audio: &[u8]) -> Vec<u8> {
        if self.tempo == 1.0 {
            return audio.to_vec();
        }
        self.input.extend(audio.iter().map(|b| f32::from(decode(*b))));
        while self.frame_ready() {
            self.add_frame();
        }
        self.emit(self.frame * HS)
    }

    /// The rest, once the stream has ended.
    pub fn finish(mut self) -> Vec<u8> {
        if self.tempo == 1.0 {
            return Vec::new();
        }
        let heard = self.input.len();
        let wanted = (heard as f64 / self.tempo).round() as usize;
        self.input.extend(std::iter::repeat_n(0.0, N + SEARCH + self.analysis_hop() as usize + HS));
        while self.frame * HS < wanted + HS && self.frame_ready() {
            self.add_frame();
        }
        let end = wanted.min(self.output.len());
        self.emit(end)
    }

    fn nominal(&self, frame: usize) -> usize {
        (frame as f64 * self.analysis_hop()).round() as usize
    }

    fn frame_ready(&self) -> bool {
        let reach = self.nominal(self.frame) + SEARCH + N;
        let natural = self.previous.map_or(0, |p| p + HS + N);
        self.input.len() >= reach.max(natural)
    }

    fn add_frame(&mut self) {
        let nominal = self.nominal(self.frame);
        let start = match self.previous {
            None => nominal,
            Some(previous) => {
                // Where the last window's speech would naturally go on.
                let natural = &self.input[previous + HS..previous + HS + N];
                let from = nominal.saturating_sub(SEARCH);
                let to = nominal + SEARCH;
                let mut best = (f32::MIN, nominal);
                for candidate in from..=to {
                    let segment = &self.input[candidate..candidate + N];
                    let score: f32 = natural.iter().zip(segment).map(|(a, b)| a * b).sum();
                    if score > best.0 {
                        best = (score, candidate);
                    }
                }
                best.1
            }
        };
        let at = self.frame * HS;
        if self.output.len() < at + N {
            self.output.resize(at + N, 0.0);
        }
        for n in 0..N {
            // The first window starts at full level, not faded in from silence.
            let w = if self.frame == 0 && n < HS { 1.0 } else { self.window[n] };
            self.output[at + n] += w * self.input[start + n];
        }
        self.previous = Some(start);
        self.frame += 1;
    }

    fn emit(&mut self, upto: usize) -> Vec<u8> {
        let upto = upto.min(self.output.len());
        if upto <= self.emitted {
            return Vec::new();
        }
        let out = self.output[self.emitted..upto]
            .iter()
            .map(|s| encode(s.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16))
            .collect();
        self.emitted = upto;
        out
    }
}

/// A whole clip at `tempo`.
pub fn stretch(audio: &[u8], tempo: f32) -> Vec<u8> {
    let mut t = Tempo::new(tempo);
    let mut out = t.push(audio);
    out.extend(t.finish());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tone that rises and falls, as speech does, in μ-law.
    fn speech(seconds: f32) -> Vec<u8> {
        let n = (8000.0 * seconds) as usize;
        (0..n)
            .map(|i| {
                let t = i as f32 / 8000.0;
                let envelope = (std::f32::consts::PI * t * 3.0).sin().abs();
                encode((envelope * 8000.0 * (2.0 * std::f32::consts::PI * 180.0 * t).sin()) as i16)
            })
            .collect()
    }

    fn pitch(audio: &[u8]) -> f32 {
        // Zero crossings per second, halved: the tone's frequency.
        let s: Vec<i16> = audio.iter().map(|b| decode(*b)).collect();
        let crossings = s.windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count();
        crossings as f32 / (s.len() as f32 / 8000.0) / 2.0
    }

    #[test]
    fn faster_and_the_same_voice() {
        let clip = speech(2.0);
        let fast = stretch(&clip, 1.25);
        let ratio = clip.len() as f32 / fast.len() as f32;
        assert!((ratio - 1.25).abs() < 0.03, "{ratio}");
        let (before, after) = (pitch(&clip), pitch(&fast));
        assert!((before - after).abs() / before < 0.08, "pitch {before} -> {after}");
    }

    #[test]
    fn a_stream_in_pieces_is_the_whole_clip() {
        let clip = speech(1.5);
        let whole = stretch(&clip, 1.25);
        let mut t = Tempo::new(1.25);
        let mut pieces = Vec::new();
        for chunk in clip.chunks(733) {
            pieces.extend(t.push(chunk));
        }
        pieces.extend(t.finish());
        assert_eq!(pieces, whole, "the same audio, however it arrives");
    }

    #[test]
    fn tempo_one_changes_nothing() {
        let clip = speech(0.5);
        assert_eq!(stretch(&clip, 1.0), clip);
    }
}

/// TEMPO_IN (μ-law 8 kHz) at TEMPO_RATE into TEMPO_OUT: to listen to the result.
#[cfg(test)]
#[test]
#[ignore]
fn stretch_a_file() {
    let input = std::fs::read(std::env::var("TEMPO_IN").expect("TEMPO_IN")).expect("read");
    let rate: f32 = std::env::var("TEMPO_RATE").ok().and_then(|r| r.parse().ok()).unwrap_or(1.25);
    std::fs::write(std::env::var("TEMPO_OUT").expect("TEMPO_OUT"), stretch(&input, rate)).expect("write");
}

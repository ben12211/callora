//! G.711 μ-law, the format Twilio Media Streams carry: 8 kHz, one byte per sample, so a
//! 20 ms frame is 160 bytes. Everything Callora plays is stored and sent in this format,
//! so nothing is transcoded on the hot path.

pub const SAMPLE_RATE: u32 = 8000;
pub const FRAME_MS: u64 = 20;
pub const FRAME_BYTES: usize = 160;
/// Digital silence in μ-law.
pub const SILENCE: u8 = 0xFF;

const BIAS: i32 = 0x84;
const CLIP: i32 = 32635;

pub fn decode(byte: u8) -> i16 {
    let value = !byte;
    let sign = value & 0x80;
    let exponent = (value >> 4) & 0x07;
    let mantissa = value & 0x0F;
    let magnitude = ((i32::from(mantissa) << 3) + BIAS) << exponent;
    let sample = magnitude - BIAS;
    (if sign != 0 { -sample } else { sample }) as i16
}

pub fn encode(sample: i16) -> u8 {
    let mut s = i32::from(sample);
    let sign = if s < 0 {
        s = -s;
        0x80
    } else {
        0
    };
    s = s.min(CLIP) + BIAS;
    let mut exponent = 7;
    let mut mask = 0x4000;
    while exponent > 0 && s & mask == 0 {
        exponent -= 1;
        mask >>= 1;
    }
    let mantissa = (s >> (exponent + 3)) & 0x0F;
    !(sign | (exponent << 4) | mantissa) as u8
}

/// μ-law audio as a 16-bit PCM WAV file, which every browser plays (for listening to
/// recorded utterances; never on a call's path).
pub fn to_wav(mulaw: &[u8]) -> Vec<u8> {
    let data_len = (mulaw.len() * 2) as u32;
    let mut wav = Vec::with_capacity(44 + mulaw.len() * 2);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes()); // bytes per second
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for b in mulaw {
        wav.extend_from_slice(&decode(*b).to_le_bytes());
    }
    wav
}

/// Root-mean-square level on the 16-bit linear scale.
pub fn rms(frame: &[u8]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    let sum: f64 = frame.iter().map(|b| f64::from(decode(*b)).powi(2)).sum();
    (sum / frame.len() as f64).sqrt() as f32
}

/// Scale the level by `db` decibels (clipped).
pub fn apply_gain(audio: &mut [u8], db: f32) {
    if db.abs() < 0.01 {
        return;
    }
    let factor = 10f32.powf(db / 20.0);
    for b in audio.iter_mut() {
        let s = (f32::from(decode(*b)) * factor).clamp(-32768.0, 32767.0);
        *b = encode(s as i16);
    }
}

/// Gain with protection against clipping: a peak limiter that works across frames.
///
/// Plain gain clips the loudest samples (harsh distortion on a phone speaker). Here a sample
/// that would pass the ceiling pulls the gain down at once (attack), and the gain then
/// recovers over about 50 ms (release), so a loud syllable is turned down smoothly instead
/// of being cut flat. One limiter per item played, so its state never leaks between items.
/// Zero gain is a bit-exact pass-through: nothing recorded is touched unless asked.
#[derive(Debug, Clone)]
pub struct Limiter {
    ceiling: f32,
    release: f32,
    /// Multiplier applied on top of the gain, 1.0 when nothing is being limited.
    reduction: f32,
}

/// The loudest level μ-law can carry (see [`encode`]).
const FULL_SCALE: f32 = 32124.0;

impl Limiter {
    /// `ceiling_dbfs` is the peak the output may not pass (relative to full scale, so -1.5).
    pub fn new(ceiling_dbfs: f32) -> Self {
        let ceiling = (FULL_SCALE * 10f32.powf(ceiling_dbfs.min(0.0) / 20.0)).max(1.0);
        // 50 ms at 8 kHz.
        let release = 1.0 - (-1.0f32 / 400.0).exp();
        Self { ceiling, release, reduction: 1.0 }
    }

    /// Apply `db` of gain to a μ-law buffer without letting it pass the ceiling.
    pub fn process(&mut self, audio: &mut [u8], db: f32) {
        if db.abs() < 0.01 {
            return;
        }
        let factor = 10f32.powf(db / 20.0);
        for b in audio.iter_mut() {
            let x = f32::from(decode(*b)) * factor;
            let needed = if x.abs() > self.ceiling { self.ceiling / x.abs() } else { 1.0 };
            if needed < self.reduction {
                self.reduction = needed;
            } else {
                self.reduction += (1.0 - self.reduction) * self.release;
            }
            let y = (x * self.reduction.min(needed)).clamp(-FULL_SCALE, FULL_SCALE);
            *b = encode(y as i16);
        }
    }
}

/// Where speech starts and ends in a μ-law buffer, as byte offsets: the first and the last
/// 10 ms window louder than `threshold_rms`, widened by `head_pad_ms` before and `tail_pad_ms` after (word endings fade out, so the tail
/// keeps more). `None` when
/// nothing is louder (all silence).
pub fn speech_bounds(audio: &[u8], threshold_rms: f32, head_pad_ms: u64, tail_pad_ms: u64) -> Option<(usize, usize)> {
    const WINDOW: usize = 80; // 10 ms
    let loud = |w: &[u8]| rms(w) >= threshold_rms;
    let first = audio.chunks(WINDOW).position(loud)?;
    let last = audio.chunks(WINDOW).rposition(loud)?;
    let start = (first * WINDOW).saturating_sub(head_pad_ms as usize * 8);
    let end = ((last + 1) * WINDOW + tail_pad_ms as usize * 8).min(audio.len());
    Some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_holds_the_decoded_samples() {
        let wav = to_wav(&[SILENCE, encode(1000)]);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 4, "two 16-bit samples");
        assert_eq!(i16::from_le_bytes([wav[44], wav[45]]), 0);
        assert!((i16::from_le_bytes([wav[46], wav[47]]) - 1000).abs() < 40);
    }

    #[test]
    fn round_trip_is_close() {
        for s in [-30000i16, -1000, -1, 0, 1, 100, 1000, 30000] {
            let back = decode(encode(s));
            assert!((i32::from(back) - i32::from(s)).abs() <= (i32::from(s).abs() / 16).max(8), "{s} -> {back}");
        }
        assert_eq!(decode(SILENCE), 0);
    }

    #[test]
    fn gain_makes_it_louder() {
        let mut a = vec![encode(1000); 160];
        let before = rms(&a);
        apply_gain(&mut a, 6.0);
        assert!(rms(&a) > before * 1.8);
    }

    #[test]
    fn limiter_passes_zero_gain_untouched_and_never_passes_the_ceiling() {
        let tone: Vec<u8> = (0..800).map(|i| encode(if (i / 4) % 2 == 0 { 12000 } else { -12000 })).collect();
        let mut same = tone.clone();
        Limiter::new(-1.5).process(&mut same, 0.0);
        assert_eq!(same, tone, "0 dB is bit-exact");
        let mut loud = tone.clone();
        Limiter::new(-1.5).process(&mut loud, 12.0);
        let ceiling = 32124.0 * 10f32.powf(-1.5 / 20.0);
        let peak = loud.iter().map(|b| i32::from(decode(*b)).abs()).max().unwrap();
        assert!(peak as f32 <= ceiling * 1.03, "peak {peak} over the ceiling {ceiling}");
        assert!(rms(&loud) > rms(&tone), "still louder than the original");
    }

    #[test]
    fn limiter_gain_recovers_after_a_peak_and_state_carries_across_frames() {
        let mut lim = Limiter::new(-1.5);
        let mut peak = vec![encode(30000); 160];
        lim.process(&mut peak, 6.0);
        let mut quiet = vec![encode(2000); 160];
        let before = rms(&quiet);
        lim.process(&mut quiet, 6.0);
        assert!(rms(&quiet) > before, "recovers to a boost");
        assert!(rms(&quiet) < before * 2.05, "never past the plain 6 dB");
    }

    #[test]
    fn speech_bounds_trim_silence_and_keep_a_pad() {
        let mut a = vec![SILENCE; 800];
        a.extend(vec![encode(4000); 800]);
        a.extend(vec![SILENCE; 800]);
        let (s, e) = speech_bounds(&a, 100.0, 5, 10).unwrap();
        assert_eq!((s, e), (800 - 40, 1600 + 80));
        assert!(speech_bounds(&vec![SILENCE; 800], 100.0, 5, 10).is_none());
    }
}

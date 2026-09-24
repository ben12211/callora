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

#[cfg(test)]
mod tests {
    use super::*;

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
}

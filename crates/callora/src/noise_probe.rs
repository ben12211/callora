//! `callora noise-probe`: recorded caller audio in noise, through the caller's side of a call
//! (the VAD, optionally RNNoise, and the speech recognizer as configured), scored against what
//! was really said.
//!
//! Each line of the file is a clip: `kind` (`speech`, `side`, `noise`, `side_only`, `echo`),
//! `truth` (the caller's words), `other` (someone else's words in it), the speech's
//! `speech_start_ms`/`speech_end_ms`, and `audio` (base64 μ-law 8 kHz). What is reported:
//! - for the caller's words: how many of them came out (numbers compared as digits), and how
//!   many of the other person's words came out with them;
//! - for clips with no caller words: how often anything at all was transcribed;
//! - how long after the end of speech the VAD ended the utterance.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use base64::Engine as _;
use futures::StreamExt;

use callora_audio::denoise::Denoiser;
use callora_audio::vad::{Vad, VadConfig, VadEvent};
use callora_runtime::ports::{SpeechToText, SttEvent, SttInput};

/// Audio sent from before the VAD heard speech start, as a call keeps (300 ms).
const PREROLL_FRAMES: usize = 15;

fn words(text: &str) -> Vec<String> {
    callora_core::text::normalize(&callora_core::hebrew::with_digits(text))
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

/// How many of `said` are in `heard`, each heard word used once.
fn found(said: &[String], heard: &[String]) -> usize {
    let mut pool = heard.to_vec();
    said.iter()
        .filter(|w| {
            if let Some(i) = pool.iter().position(|h| h == *w) {
                pool.remove(i);
                true
            } else {
                false
            }
        })
        .count()
}

struct Clip {
    kind: String,
    truth: String,
    other: String,
    heard: Vec<String>,
    unsure: Vec<(String, f32)>,
    end_delay_ms: Option<i64>,
}

async fn hear(
    stt: Arc<dyn SpeechToText>,
    row: serde_json::Value,
    vad_cfg: VadConfig,
    voice: bool,
    clean: bool,
) -> anyhow::Result<Clip> {
    let audio = base64::engine::general_purpose::STANDARD.decode(row["audio"].as_str().unwrap_or(""))?;
    let frames: Vec<&[u8]> = audio.chunks(160).collect();
    let mut vad = Vad::new(vad_cfg);
    let mut denoiser = (voice || clean).then(Denoiser::new);
    let mut cleaned = Vec::with_capacity(frames.len());
    let mut segments = Vec::new();
    let mut start = 0;
    for (i, f) in frames.iter().enumerate() {
        let c = denoiser.as_mut().map(|d| d.push(f));
        let p = c.as_ref().map(|c| c.voice).filter(|_| voice);
        cleaned.push(c.map(|c| c.audio).unwrap_or_else(|| f.to_vec()));
        match vad.push_with(f, p) {
            Some(VadEvent::SpeechStarted) => start = i,
            Some(VadEvent::SpeechEnded) => segments.push((start, i)),
            None => {}
        }
    }
    if vad.is_speaking() {
        segments.push((start, frames.len()));
    }
    let speech_end_ms = row["speech_end_ms"].as_u64().unwrap_or(0);
    let speech_start_ms = row["speech_start_ms"].as_u64().unwrap_or(0);
    let end_delay_ms = segments
        .iter()
        .find(|(a, b)| {
            (*a as u64 * 20) <= speech_end_ms
                && (*b as u64 * 20) + 600 >= speech_end_ms
                && (*b as u64 * 20) >= speech_start_ms
        })
        .map(|(_, b)| *b as i64 * 20 - speech_end_ms as i64);
    let mut heard = Vec::new();
    let mut unsure = Vec::new();
    if !segments.is_empty() {
        let mut session = stt.open("he-IL", &[]).await?;
        for (a, b) in &segments {
            for f in &cleaned[a.saturating_sub(PREROLL_FRAMES)..*b] {
                session.input.send(SttInput::Audio(bytes::Bytes::copy_from_slice(f))).await?;
            }
            session.input.send(SttInput::Finalize).await?;
            loop {
                match tokio::time::timeout(Duration::from_secs(8), session.events.recv()).await {
                    Ok(Some(SttEvent::Final(t))) => {
                        heard.extend(words(&t));
                        break;
                    }
                    Ok(Some(SttEvent::Partial(_))) => {}
                    Ok(Some(SttEvent::Unsure(u))) => {
                        unsure.extend(u.into_iter().flat_map(|(w, p)| words(&w).into_iter().map(move |w| (w, p))))
                    }
                    Ok(Some(SttEvent::Error(e))) => anyhow::bail!("recognition error: {e}"),
                    _ => break,
                }
            }
        }
        let _ = session.input.send(SttInput::Close).await;
    }
    Ok(Clip {
        kind: row["kind"].as_str().unwrap_or("").to_string(),
        truth: row["truth"].as_str().unwrap_or("").to_string(),
        other: row["other"].as_str().unwrap_or("").to_string(),
        heard,
        unsure,
        end_delay_ms,
    })
}

pub async fn run(
    stt: Arc<dyn SpeechToText>,
    file: &Path,
    vad_cfg: VadConfig,
    voice: bool,
    clean: bool,
    show: bool,
) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let rows: Vec<serde_json::Value> = text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    println!(
        "recognizer {} | VAD {} | {} | {} clips",
        stt.name(),
        if voice {
            "noise-following with RNNoise voice"
        } else if vad_cfg.noise_floor_ratio > 0.0 {
            "noise-following"
        } else {
            "energy"
        },
        if clean { "RNNoise-cleaned audio to the recognizer" } else { "the call's audio to the recognizer" },
        rows.len()
    );
    let clips: Vec<anyhow::Result<Clip>> = futures::stream::iter(rows)
        .map(|row| hear(stt.clone(), row, vad_cfg, voice, clean))
        .buffer_unordered(4)
        .collect()
        .await;
    let mut by_kind: std::collections::BTreeMap<String, Vec<Clip>> = Default::default();
    let mut errors = 0;
    for c in clips {
        match c {
            Ok(c) => by_kind.entry(c.kind.clone()).or_default().push(c),
            Err(e) => {
                errors += 1;
                eprintln!("error: {e:#}");
            }
        }
    }
    for (kind, clips) in &by_kind {
        let n = clips.len();
        if kind == "speech" || kind == "side" {
            let (mut said, mut got, mut whole, mut other, mut other_got) = (0, 0, 0, 0, 0);
            let mut delays: Vec<i64> = Vec::new();
            for c in clips {
                let truth = words(&c.truth);
                let f = found(&truth, &c.heard);
                said += truth.len();
                got += f;
                whole += usize::from(f == truth.len());
                let o = words(&c.other);
                other += o.len();
                other_got += found(&o, &c.heard);
                delays.extend(c.end_delay_ms);
                if show && f < truth.len() {
                    println!("  {kind} | said \"{}\" | heard \"{}\"", c.truth, c.heard.join(" "));
                }
            }
            // Unsure words: how many were wrong (not the caller's), and how many wrong words were
            // flagged.
            let wrong: usize =
                clips.iter().map(|c| c.heard.iter().filter(|w| !words(&c.truth).contains(w)).count()).sum();
            for below in [0.5f32, 0.7, 0.8, 0.9, 0.95] {
                let (mut flagged, mut flagged_wrong) = (0, 0);
                for c in clips {
                    let truth = words(&c.truth);
                    for (w, _) in c.unsure.iter().filter(|(_, p)| *p < below) {
                        flagged += 1;
                        flagged_wrong += usize::from(!truth.contains(w));
                    }
                }
                if flagged > 0 {
                    println!(
                        "{kind:>9}: unsure under {below}: {flagged} words, {flagged_wrong} of them wrong | {flagged_wrong} of {wrong} wrong words flagged"
                    );
                }
            }
            delays.sort_unstable();
            let p50 = delays.get(delays.len() / 2).copied().unwrap_or(0);
            println!(
                "{kind:>9}: {n} clips | caller's words heard {got}/{said} ({:.0}%) | every word {whole}/{n} | end of speech heard after {p50} ms (median){}",
                100.0 * got as f64 / said.max(1) as f64,
                if other > 0 { format!(" | other person's words in it {other_got}/{other} ({:.0}%)", 100.0 * other_got as f64 / other as f64) } else { String::new() }
            );
        } else {
            let worded = clips.iter().filter(|c| !c.heard.is_empty()).count();
            let other: usize = clips.iter().map(|c| words(&c.other).len()).sum();
            let other_got: usize = clips.iter().map(|c| found(&words(&c.other), &c.heard)).sum();
            if show {
                for c in clips.iter().filter(|c| !c.heard.is_empty()) {
                    println!("  {kind} | heard \"{}\"", c.heard.join(" "));
                }
            }
            println!(
                "{kind:>9}: {n} clips | words came out of {worded}/{n}{}",
                if other > 0 { format!(" | the other person's words {other_got}/{other}") } else { String::new() }
            );
        }
    }
    if errors > 0 {
        println!("{errors} clips failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_count_the_same_in_words_and_digits() {
        let said = words("בן זכאי ארבעים וחמש");
        assert_eq!(found(&said, &words("בן זכאי 45.")), said.len());
        assert_eq!(found(&words("כן כן"), &words("כן")), 1, "each heard word once");
    }
}

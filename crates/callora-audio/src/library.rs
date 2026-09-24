//! The pre-generated voice library.
//!
//! On disk: `<root>/<business_id>/manifest.json` and one raw μ-law file per clip, named by
//! a content hash of (delivery, text). At startup the whole library is loaded into memory,
//! so playing a cached response costs a hash lookup and nothing else.
//!
//! The manifest records the voice and model it was generated with. A library generated for
//! a different voice is ignored (and reported), rather than mixing two voices in a call.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use callora_core::business::Business;
use callora_core::render::{library_entries, LibraryEntry};
use callora_core::speech::prepare_for_tts;

use crate::tts::{Synthesizer, TtsRequest};

pub const FORMAT: &str = "mulaw_8000";

pub fn clip_key(delivery: &str, text: &str) -> String {
    let mut h = Sha256::new();
    h.update(delivery.as_bytes());
    h.update([0x1f]);
    h.update(text.trim().as_bytes());
    hex::encode(&h.finalize()[..12])
}

/// The same sentence, ignoring punctuation and spacing: an agent that says "לאן נוסעים"
/// instead of "לאן נוסעים?" still gets the pre-recorded clip.
fn loose_key(delivery: &str, text: &str) -> String {
    let words: Vec<String> = text
        .split_whitespace()
        .map(|w| w.chars().filter(|c| c.is_alphanumeric() || *c == '\'').collect::<String>())
        .filter(|w| !w.is_empty())
        .collect();
    format!("{delivery}\u{1f}{}", words.join(" "))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManifestEntry {
    pub key: String,
    pub response_id: String,
    pub delivery: String,
    pub text: String,
    /// The text actually sent to TTS (after pronunciation and normalization).
    pub spoken: String,
    pub file: String,
    pub bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub business_id: String,
    pub voice_id: String,
    pub model: String,
    pub format: String,
    pub entries: Vec<ManifestEntry>,
}

/// A business's clips, in memory.
#[derive(Debug, Default)]
pub struct VoiceLibrary {
    clips: HashMap<String, Bytes>,
    /// Loose key → clip key.
    loose: HashMap<String, String>,
    pub voice_id: Option<String>,
    pub model: Option<String>,
}

impl VoiceLibrary {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.clips.len()
    }

    pub fn is_empty(&self) -> bool {
        self.clips.is_empty()
    }

    /// The clip for this exact sentence in this delivery. A missing non-normal delivery
    /// falls back to the normal clip: instant audio beats a slower-but-exact synthesis.
    pub fn get(&self, delivery: &str, text: &str) -> Option<Bytes> {
        self.clips
            .get(&clip_key(delivery, text))
            .or_else(|| (delivery != "normal").then(|| self.clips.get(&clip_key("normal", text))).flatten())
            .cloned()
    }

    /// Like [`VoiceLibrary::get`], but also matches the sentence with different
    /// punctuation or spacing. For free text, whose exact form is not known in advance.
    pub fn get_loose(&self, delivery: &str, text: &str) -> Option<Bytes> {
        self.get(delivery, text).or_else(|| {
            let find = |d: &str| self.loose.get(&loose_key(d, text)).and_then(|k| self.clips.get(k));
            find(delivery).or_else(|| (delivery != "normal").then(|| find("normal")).flatten()).cloned()
        })
    }

    pub fn insert(&mut self, delivery: &str, text: &str, audio: Bytes) {
        let key = clip_key(delivery, text);
        self.loose.insert(loose_key(delivery, text), key.clone());
        self.clips.insert(key, audio);
    }

    /// Load `<root>/<business>/`. A missing library is not an error (every response then
    /// goes through dynamic TTS); a library for another voice is ignored.
    pub fn load(root: &Path, business: &Business) -> anyhow::Result<Self> {
        let dir = root.join(&business.config.id);
        let manifest_path = dir.join("manifest.json");
        if !manifest_path.exists() {
            tracing::warn!(business = %business.config.id, path = %manifest_path.display(), "no voice library; every response will use dynamic TTS");
            return Ok(Self::empty());
        }
        let manifest: Manifest = serde_json::from_slice(&std::fs::read(&manifest_path)?)?;
        if let Some(voice) = &business.voice_id {
            if &manifest.voice_id != voice {
                tracing::error!(business = %business.config.id, "voice library was generated for a different voice; ignoring it (rebuild with `callora voice-library build`)");
                return Ok(Self::empty());
            }
        }
        let mut lib = Self {
            clips: HashMap::new(),
            loose: HashMap::new(),
            voice_id: Some(manifest.voice_id.clone()),
            model: Some(manifest.model.clone()),
        };
        for e in &manifest.entries {
            match std::fs::read(dir.join(&e.file)) {
                Ok(bytes) => {
                    lib.loose.insert(loose_key(&e.delivery, &e.text), e.key.clone());
                    lib.clips.insert(e.key.clone(), Bytes::from(bytes));
                }
                Err(err) => tracing::warn!(file = %e.file, %err, "voice library clip missing"),
            }
        }
        let wanted = library_entries(business).len();
        tracing::info!(business = %business.config.id, clips = lib.len(), wanted, "voice library loaded");
        Ok(lib)
    }
}

/// What a library build did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BuildReport {
    pub total: usize,
    pub generated: usize,
    pub reused: usize,
    pub failed: Vec<String>,
}

pub struct LibraryBuilder<'a> {
    pub business: &'a Business,
    pub synthesizer: Arc<dyn Synthesizer>,
    pub voice_id: String,
    pub model: String,
    pub root: PathBuf,
    pub concurrency: usize,
}

impl LibraryBuilder<'_> {
    /// Generate every missing clip. Clips already on disk for the same voice, model and
    /// text are reused, so re-running after a config change only synthesizes what changed.
    pub async fn build(&self) -> anyhow::Result<BuildReport> {
        let b = self.business;
        let dir = self.root.join(&b.config.id);
        std::fs::create_dir_all(&dir)?;
        let previous: Option<Manifest> =
            std::fs::read(dir.join("manifest.json")).ok().and_then(|raw| serde_json::from_slice(&raw).ok());
        let reusable: HashMap<String, ManifestEntry> = previous
            .filter(|m| m.voice_id == self.voice_id && m.model == self.model)
            .map(|m| m.entries.into_iter().filter(|e| dir.join(&e.file).exists()).map(|e| (e.key.clone(), e)).collect())
            .unwrap_or_default();

        let entries: Vec<LibraryEntry> = library_entries(b);
        let mut report = BuildReport { total: entries.len(), ..Default::default() };
        let mut done: Vec<ManifestEntry> = Vec::new();
        let mut todo = Vec::new();
        for e in entries {
            let key = clip_key(&e.delivery, &e.text);
            let spoken = prepare_for_tts(&e.text, &b.config.language, &b.pronouncer);
            match reusable.get(&key) {
                Some(prev) if prev.spoken == spoken => {
                    report.reused += 1;
                    done.push(prev.clone());
                }
                _ => todo.push((key, e, spoken)),
            }
        }

        let results: Vec<(String, LibraryEntry, String, anyhow::Result<Bytes>)> =
            futures::stream::iter(todo.into_iter().map(|(key, e, spoken)| {
                let synth = self.synthesizer.clone();
                let req = TtsRequest {
                    text: spoken.clone(),
                    voice_id: self.voice_id.clone(),
                    model: self.model.clone(),
                    settings: b.config.voice.settings_for(&e.delivery),
                    language: b.config.language.clone(),
                };
                async move {
                    let audio = async {
                        let stream = synth.synthesize(req).await?;
                        let chunks: Vec<Bytes> = stream.try_collect().await?;
                        anyhow::Ok(Bytes::from(chunks.concat()))
                    }
                    .await;
                    (key, e, spoken, audio)
                }
            }))
            .buffer_unordered(self.concurrency.max(1))
            .collect()
            .await;

        for (key, e, spoken, audio) in results {
            match audio {
                Ok(audio) if !audio.is_empty() => {
                    let file = format!("{key}.ulaw");
                    std::fs::write(dir.join(&file), &audio)?;
                    report.generated += 1;
                    done.push(ManifestEntry {
                        key,
                        response_id: e.response_id,
                        delivery: e.delivery,
                        text: e.text,
                        spoken,
                        file,
                        bytes: audio.len(),
                    });
                }
                Ok(_) => report.failed.push(format!("{}: empty audio", e.text)),
                Err(err) => report.failed.push(format!("{}: {err:#}", e.text)),
            }
        }
        done.sort_by(|a, b| (&a.response_id, &a.delivery, &a.text).cmp(&(&b.response_id, &b.delivery, &b.text)));
        let manifest = Manifest {
            business_id: b.config.id.clone(),
            voice_id: self.voice_id.clone(),
            model: self.model.clone(),
            format: FORMAT.into(),
            entries: done,
        };
        let tmp = dir.join("manifest.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&manifest)?)?;
        std::fs::rename(&tmp, dir.join("manifest.json"))?;
        Ok(report)
    }
}

//! Text-to-speech as the runtime sees it: a streaming synthesizer of μ-law 8 kHz, plus an
//! in-memory cache so a dynamic sentence synthesized once (a customer's name, an address
//! read back twice) is instant the second time.

use std::num::NonZeroUsize;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use callora_core::config::VoiceSettings;

#[derive(Debug, Clone, PartialEq)]
pub struct TtsRequest {
    /// Already prepared for speech (pronunciations applied, numbers in words).
    pub text: String,
    pub voice_id: String,
    pub model: String,
    pub settings: VoiceSettings,
    /// BCP-47 locale of the business.
    pub language: String,
}

impl TtsRequest {
    /// Identity of the audio this request produces.
    pub fn cache_key(&self) -> String {
        let s = &self.settings;
        let mut h = Sha256::new();
        h.update(format!(
            "{}\u{1f}{}\u{1f}{}\u{1f}{:.3}/{:.3}/{:.3}/{:.3}\u{1f}{}",
            self.voice_id, self.model, self.language, s.stability, s.similarity_boost, s.style, s.speed, self.text
        ));
        hex::encode(&h.finalize()[..16])
    }
}

pub type AudioStream = BoxStream<'static, anyhow::Result<Bytes>>;

/// A streaming TTS engine producing raw μ-law 8 kHz.
#[async_trait]
pub trait Synthesizer: Send + Sync {
    /// Starts synthesis. The first chunk should arrive as early as the provider allows.
    async fn synthesize(&self, request: TtsRequest) -> anyhow::Result<AudioStream>;

    fn name(&self) -> &'static str;
}

/// Bounded LRU of complete dynamic syntheses, shared by every call in the process.
#[derive(Clone)]
pub struct TtsCache {
    inner: Arc<Mutex<lru::LruCache<String, Bytes>>>,
}

impl TtsCache {
    pub fn new(entries: usize) -> Self {
        let cap = NonZeroUsize::new(entries.max(1)).unwrap_or(NonZeroUsize::MIN);
        Self { inner: Arc::new(Mutex::new(lru::LruCache::new(cap))) }
    }

    pub fn get(&self, key: &str) -> Option<Bytes> {
        self.inner.lock().get(key).cloned()
    }

    pub fn put(&self, key: String, audio: Bytes) {
        self.inner.lock().put(key, audio);
    }
}

//! ElevenLabs Scribe v2 (not realtime): one utterance transcribed on its own, as a second
//! hearing when the realtime transcript is doubtful. Slower (~0.5 s for a short sentence)
//! but more accurate, and it takes up to 1,000 keyterms: every street of the caller's city.
//!
//! The caller's μ-law 8 kHz becomes 16-bit PCM at 16 kHz (`pcm_s16le_16`), the one raw
//! format the service takes. The API key travels only in the `xi-api-key` header.

use async_trait::async_trait;

use callora_runtime::ports::Transcriber;

pub const DEFAULT_BASE_URL: &str = "https://api.elevenlabs.io";
pub const DEFAULT_MODEL: &str = "scribe_v2";
const MAX_KEYTERMS: usize = 1000;
const MAX_KEYTERM_CHARS: usize = 49;

pub struct ScribeBatch {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
}

impl ScribeBatch {
    pub fn new(http: reqwest::Client, api_key: String, base_url: Option<String>, model: Option<String>) -> Self {
        let nonblank = |v: Option<String>| v.filter(|s| !s.trim().is_empty());
        Self {
            http,
            api_key,
            base_url: nonblank(base_url).unwrap_or_else(|| DEFAULT_BASE_URL.into()).trim_end_matches('/').into(),
            model: nonblank(model).unwrap_or_else(|| DEFAULT_MODEL.into()),
        }
    }
}

/// μ-law 8 kHz → 16-bit little-endian PCM 16 kHz (each sample, then the midpoint to the next).
pub fn pcm16k(mulaw: &[u8]) -> Vec<u8> {
    let samples: Vec<i16> = mulaw.iter().map(|&b| callora_audio::mulaw::decode(b)).collect();
    let mut out = Vec::with_capacity(samples.len() * 4);
    for (i, &s) in samples.iter().enumerate() {
        let next = samples.get(i + 1).copied().unwrap_or(s);
        let mid = ((i32::from(s) + i32::from(next)) / 2) as i16;
        out.extend_from_slice(&s.to_le_bytes());
        out.extend_from_slice(&mid.to_le_bytes());
    }
    out
}

/// Keyterms the service accepts: at most 1,000, each under 50 characters and five words.
pub fn usable_keyterms(keyterms: &[String]) -> Vec<String> {
    keyterms
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty() && t.chars().count() <= MAX_KEYTERM_CHARS && t.split_whitespace().count() <= 5)
        .take(MAX_KEYTERMS)
        .map(str::to_string)
        .collect()
}

#[async_trait]
impl Transcriber for ScribeBatch {
    async fn transcribe(&self, mulaw: &[u8], language: &str, keyterms: &[String]) -> anyhow::Result<String> {
        let lang = language.split('-').next().unwrap_or(language).to_string();
        let file = reqwest::multipart::Part::bytes(pcm16k(mulaw)).file_name("utterance.pcm");
        let mut form = reqwest::multipart::Form::new()
            .text("model_id", self.model.clone())
            .text("language_code", lang)
            .text("file_format", "pcm_s16le_16")
            .text("tag_audio_events", "false")
            .part("file", file);
        for term in usable_keyterms(keyterms) {
            form = form.text("keyterms", term);
        }
        let response = self
            .http
            .post(format!("{}/v1/speech-to-text", self.base_url))
            .header("xi-api-key", &self.api_key)
            .multipart(form)
            .send()
            .await?;
        let status = response.status();
        let body: serde_json::Value = response.json().await?;
        if !status.is_success() {
            anyhow::bail!("scribe {status}: {body}");
        }
        Ok(body.get("text").and_then(|t| t.as_str()).unwrap_or("").trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_is_twice_the_samples_little_endian() {
        let pcm = pcm16k(&[0xFF, 0xFF, 0x00]);
        assert_eq!(pcm.len(), 3 * 2 * 2, "two 16-bit samples for each μ-law byte");
        // 0xFF is μ-law silence (0), 0x00 the most negative sample.
        assert_eq!(&pcm[..4], &[0, 0, 0, 0]);
        assert!(i16::from_le_bytes([pcm[8], pcm[9]]) < -30_000);
    }

    #[test]
    fn keyterms_are_trimmed_to_what_the_service_takes() {
        let mut terms: Vec<String> = (0..1200).map(|i| format!("רחוב {i}")).collect();
        terms.push("א".repeat(60));
        terms.push("אחת שתיים שלוש ארבע חמש שש".into());
        let kept = usable_keyterms(&terms);
        assert_eq!(kept.len(), 1000);
        assert!(kept.iter().all(|t| t.chars().count() < 50));
    }
}

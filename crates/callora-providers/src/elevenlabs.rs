//! ElevenLabs text-to-speech, streamed as raw μ-law 8 kHz (`ulaw_8000`) so the audio goes
//! to Twilio untouched. Used to generate the voice library and for dynamic fallback.
//!
//! Carried over from the legacy integration: the same API key and API origin variables,
//! the `v3` models as the only ones that speak Hebrew properly (the fast v2 models produced
//! audible nonsense on Hebrew), and phone-tuned voice settings.

use async_trait::async_trait;
use futures::StreamExt;

use callora_audio::tts::{AudioStream, Synthesizer, TtsRequest};

pub const DEFAULT_BASE_URL: &str = "https://api.elevenlabs.io";

pub struct ElevenLabs {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl ElevenLabs {
    pub fn new(http: reqwest::Client, api_key: String, base_url: Option<String>) -> Self {
        let base_url = base_url.filter(|b| !b.trim().is_empty()).unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        Self { http, api_key, base_url: base_url.trim_end_matches('/').to_string() }
    }
}

/// `eleven_v3` accepts only three stability presets (creative / natural / robust).
fn stability_for(model: &str, stability: f32) -> f32 {
    if model.starts_with("eleven_v3") {
        if stability < 0.25 {
            0.0
        } else if stability < 0.75 {
            0.5
        } else {
            1.0
        }
    } else {
        stability
    }
}

/// Only the v2.5 models accept `language_code`; the others reject the request if it is set.
fn language_code(model: &str, language: &str) -> Option<String> {
    model.ends_with("v2_5").then(|| language.split('-').next().unwrap_or(language).to_string())
}

pub fn request_body(req: &TtsRequest) -> serde_json::Value {
    let s = &req.settings;
    let mut body = serde_json::json!({
        "text": req.text,
        "model_id": req.model,
        "voice_settings": {
            "stability": stability_for(&req.model, s.stability),
            "similarity_boost": s.similarity_boost,
            "style": s.style,
            "speed": s.speed,
            "use_speaker_boost": true,
        },
    });
    if let Some(code) = language_code(&req.model, &req.language) {
        body["language_code"] = serde_json::json!(code);
    }
    body
}

#[async_trait]
impl Synthesizer for ElevenLabs {
    async fn synthesize(&self, req: TtsRequest) -> anyhow::Result<AudioStream> {
        let url = format!("{}/v1/text-to-speech/{}/stream?output_format=ulaw_8000", self.base_url, req.voice_id);
        let resp = self.http.post(url).header("xi-api-key", &self.api_key).json(&request_body(&req)).send().await?;
        let status = resp.status();
        if !status.is_success() {
            // Never echo the key; the body is ElevenLabs' own error description.
            let detail = resp.text().await.unwrap_or_default();
            anyhow::bail!("ElevenLabs TTS failed with HTTP {status}: {}", detail.chars().take(300).collect::<String>());
        }
        Ok(resp.bytes_stream().map(|r| r.map_err(anyhow::Error::from)).boxed())
    }

    fn name(&self) -> &'static str {
        "elevenlabs"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use callora_core::config::VoiceSettings;

    fn req(model: &str) -> TtsRequest {
        TtsRequest {
            text: "שלום".into(),
            voice_id: "v".into(),
            model: model.into(),
            settings: VoiceSettings { stability: 0.35, similarity_boost: 0.75, style: 0.0, speed: 1.0 },
            language: "he-IL".into(),
        }
    }

    #[test]
    fn v3_snaps_stability_and_omits_language() {
        let b = request_body(&req("eleven_v3"));
        assert_eq!(b["voice_settings"]["stability"], 0.5);
        assert!(b.get("language_code").is_none());
    }

    #[test]
    fn flash_v2_5_gets_language_code() {
        let b = request_body(&req("eleven_flash_v2_5"));
        assert_eq!(b["language_code"], "he");
        assert!((b["voice_settings"]["stability"].as_f64().unwrap() - 0.35).abs() < 1e-6);
    }
}

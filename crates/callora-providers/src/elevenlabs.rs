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

/// The stability actually sent for a model. `eleven_v3` accepts only three presets
/// (creative 0.0 / natural 0.5 / robust 1.0), so 0.9 and 1.0 are the same voice there and
/// 0.5 is the next step down; the models in use now (`eleven_v4_turbo`) take the value as is.
pub fn effective_stability(model: &str, stability: f32) -> f32 {
    stability_for(model, stability)
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

/// `language_code` pins the language (names and loanwords stay Hebrew). The v2.5 and v4
/// models take it (checked on the account for `eleven_v4_turbo`); the others reject it.
fn language_code(model: &str, language: &str) -> Option<String> {
    (model.ends_with("v2_5") || model.starts_with("eleven_v4"))
        .then(|| language.split('-').next().unwrap_or(language).to_string())
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
            "use_speaker_boost": s.speaker_boost,
        },
    });
    if let Some(code) = language_code(&req.model, &req.language) {
        body["language_code"] = serde_json::json!(code);
    }
    if let Some(previous) = req.previous_text.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
        body["previous_text"] = serde_json::json!(previous);
    }
    body
}

#[async_trait]
impl Synthesizer for ElevenLabs {
    async fn warm(&self) {
        let _ = self
            .http
            .get(format!("{}/v1/models", self.base_url))
            .header("xi-api-key", &self.api_key)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await;
    }

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
            settings: VoiceSettings {
                stability: 0.35,
                similarity_boost: 0.75,
                style: 0.0,
                speed: 1.0,
                speaker_boost: true,
            },
            language: "he-IL".into(),
            previous_text: None,
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

    #[test]
    fn the_high_stability_and_natural_presets_stay_different_on_v3() {
        // 0.9 snaps to the robust preset, 0.5 is natural: the A/B compares two real voices.
        assert_eq!(effective_stability("eleven_v3", 0.9), 1.0);
        assert_eq!(effective_stability("eleven_v3", 0.5), 0.5);
        // The turbo model is continuous: 0.9 stays 0.9.
        assert!((effective_stability("eleven_v4_turbo", 0.9) - 0.9).abs() < 1e-6);
        assert!((effective_stability("eleven_v4_turbo", 0.55) - 0.55).abs() < 1e-6);
    }

    #[test]
    fn speaker_boost_is_sent_as_configured_and_on_by_default() {
        let mut r = req("eleven_v4_turbo");
        assert_eq!(request_body(&r)["voice_settings"]["use_speaker_boost"], true);
        r.settings.speaker_boost = false;
        assert_eq!(request_body(&r)["voice_settings"]["use_speaker_boost"], false);
        // A saved setting without the field (every existing library) means boost on.
        let s: VoiceSettings =
            serde_json::from_str(r#"{"stability":0.9,"similarity_boost":0.75,"speed":1.0}"#).unwrap();
        assert!(s.speaker_boost);
        assert!(!serde_json::to_string(&s).unwrap().contains("speaker_boost"), "the library manifest is unchanged");
    }

    #[test]
    fn v4_pins_hebrew_and_carries_the_sentence_before() {
        let mut r = req("eleven_v4_turbo");
        assert_eq!(request_body(&r)["language_code"], "he");
        assert!(request_body(&r).get("previous_text").is_none(), "nothing before: none sent");
        r.previous_text = Some("סגור.".into());
        assert_eq!(request_body(&r)["previous_text"], "סגור.");
        assert!(request_body(&req("eleven_v3")).get("language_code").is_none(), "v3 rejects it");
    }
}

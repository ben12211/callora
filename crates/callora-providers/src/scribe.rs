//! ElevenLabs Scribe v2 Realtime streaming speech-to-text.
//!
//! Audio goes in as μ-law 8 kHz (`ulaw_8000`), exactly what Twilio delivers, so nothing is
//! transcoded; each 20 ms frame travels base64-encoded in an `input_audio_chunk` message.
//! The API key travels only in the `xi-api-key` handshake header, never in the URL.
//!
//! Endpointing stays with the runtime's VAD (`commit_strategy=manual`): `Finalize` sends a
//! commit, and the committed transcript is the utterance. `keyterms` bias recognition
//! toward the business's place and street names, which Hebrew recognizers otherwise
//! mangle ("בני ברק" heard as "בני מון").
//!
//! On live Hebrew phone audio this replaced Cartesia ink-whisper, which dropped and merged
//! words ("מונית מבאר שבע" → "מוניטמי ביר שבע") and invented "תודה." on line noise.

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use callora_runtime::ports::{SpeechToText, SttEvent, SttInput, SttSession};

pub const DEFAULT_BASE_URL: &str = "wss://api.elevenlabs.io";
pub const DEFAULT_MODEL: &str = "scribe_v2_realtime";
/// Keyterms sent per session, and the longest one kept.
const MAX_KEYTERMS: usize = 100;
const MAX_KEYTERM_CHARS: usize = 50;

pub struct Scribe {
    api_key: String,
    base_url: String,
    model: String,
}

impl Scribe {
    pub fn new(api_key: String, base_url: Option<String>, model: Option<String>) -> Self {
        let nonblank = |v: Option<String>| v.filter(|s| !s.trim().is_empty());
        Self {
            api_key,
            base_url: nonblank(base_url).unwrap_or_else(|| DEFAULT_BASE_URL.into()).trim_end_matches('/').into(),
            model: nonblank(model).unwrap_or_else(|| DEFAULT_MODEL.into()),
        }
    }

    pub fn url(&self, language: &str, keyterms: &[String]) -> anyhow::Result<String> {
        let lang = language.split('-').next().unwrap_or(language);
        let mut url = url::Url::parse(&format!("{}/v1/speech-to-text/realtime", self.base_url))?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("model_id", &self.model)
                .append_pair("audio_format", "ulaw_8000")
                .append_pair("language_code", lang)
                .append_pair("commit_strategy", "manual");
            for term in keyterms
                .iter()
                .map(|t| t.trim())
                .filter(|t| !t.is_empty() && t.chars().count() <= MAX_KEYTERM_CHARS)
                .take(MAX_KEYTERMS)
            {
                q.append_pair("keyterms", term);
            }
        }
        Ok(url.into())
    }
}

fn audio_chunk(audio: &[u8], commit: bool) -> String {
    json!({
        "message_type": "input_audio_chunk",
        "audio_base_64": STANDARD.encode(audio),
        "commit": commit,
        "sample_rate": 8000,
    })
    .to_string()
}

/// What a server message means for the session.
fn event(v: &Value) -> Option<SttEvent> {
    let kind = v.get("message_type").and_then(Value::as_str)?;
    let text = || v.get("text").and_then(Value::as_str).unwrap_or("").trim().to_string();
    match kind {
        "partial_transcript" => Some(text()).filter(|t| !t.is_empty()).map(SttEvent::Partial),
        "committed_transcript" | "committed_transcript_with_timestamps" => {
            Some(text()).filter(|t| !t.is_empty()).map(SttEvent::Final)
        }
        // A commit with no speech in it (the VAD fired on noise) or commits too close
        // together: nothing was said, which is not an error for the call.
        "insufficient_audio_activity" | "commit_throttled" | "session_started" | "warning" => None,
        _ if v.get("error").is_some() => {
            Some(SttEvent::Error(format!("{kind}: {}", v.get("error").map(ToString::to_string).unwrap_or_default())))
        }
        _ => None,
    }
}

#[async_trait]
impl SpeechToText for Scribe {
    async fn open(&self, language: &str, keyterms: &[String]) -> anyhow::Result<SttSession> {
        let mut request = self.url(language, keyterms)?.into_client_request()?;
        request.headers_mut().insert("xi-api-key", self.api_key.parse()?);
        let (ws, _) =
            tokio::time::timeout(std::time::Duration::from_secs(8), tokio_tungstenite::connect_async(request))
                .await??;
        let (mut sink, mut stream) = ws.split();
        let (in_tx, mut in_rx) = mpsc::channel::<SttInput>(512);
        let (ev_tx, ev_rx) = mpsc::channel::<SttEvent>(64);

        tokio::spawn(async move {
            while let Some(input) = in_rx.recv().await {
                let msg = match input {
                    SttInput::Audio(a) => audio_chunk(&a, false),
                    SttInput::Finalize => audio_chunk(&[], true),
                    SttInput::Close => break,
                };
                if sink.send(Message::Text(msg.into())).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                let Message::Text(text) = msg else { continue };
                let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
                if let Some(e) = event(&v) {
                    if ev_tx.send(e).await.is_err() {
                        return;
                    }
                }
            }
            let _ = ev_tx.send(SttEvent::Closed).await;
        });

        Ok(SttSession { input: in_tx, events: ev_rx })
    }

    fn name(&self) -> &'static str {
        "scribe"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_carries_format_language_manual_commits_and_keyterms_but_not_the_key() {
        let s = Scribe::new("secret-key".into(), None, None);
        let terms = vec!["בני ברק".to_string(), " ".to_string(), "x".repeat(60)];
        let url = s.url("he-IL", &terms).unwrap();
        let parsed = url::Url::parse(&url).unwrap();
        let pairs: Vec<(String, String)> = parsed.query_pairs().into_owned().collect();
        assert!(url.starts_with("wss://api.elevenlabs.io/v1/speech-to-text/realtime?"));
        for (k, v) in [
            ("model_id", "scribe_v2_realtime"),
            ("audio_format", "ulaw_8000"),
            ("language_code", "he"),
            ("commit_strategy", "manual"),
        ] {
            assert!(pairs.contains(&(k.into(), v.into())), "{k}={v} in {url}");
        }
        let keyterms: Vec<&str> = pairs.iter().filter(|(k, _)| k == "keyterms").map(|(_, v)| v.as_str()).collect();
        assert_eq!(keyterms, vec!["בני ברק"], "blank and overlong terms are dropped");
        assert!(!url.contains("secret-key"));
    }

    #[test]
    fn audio_and_commit_messages() {
        let v: Value = serde_json::from_str(&audio_chunk(&[0xff, 0x7f], false)).unwrap();
        assert_eq!(v["message_type"], "input_audio_chunk");
        assert_eq!(v["audio_base_64"], "/38=");
        assert_eq!(v["commit"], false);
        assert_eq!(v["sample_rate"], 8000);
        let c: Value = serde_json::from_str(&audio_chunk(&[], true)).unwrap();
        assert_eq!((c["audio_base_64"].as_str(), c["commit"].as_bool()), (Some(""), Some(true)));
    }

    #[test]
    fn server_messages_become_events() {
        let ev = |j: Value| event(&j);
        assert!(matches!(ev(json!({"message_type": "committed_transcript", "text": " לתל אביב. "})),
            Some(SttEvent::Final(t)) if t == "לתל אביב."));
        assert!(matches!(ev(json!({"message_type": "partial_transcript", "text": "לתל"})),
            Some(SttEvent::Partial(t)) if t == "לתל"));
        assert!(ev(json!({"message_type": "committed_transcript", "text": "  "})).is_none(), "empty commit");
        assert!(ev(json!({"message_type": "insufficient_audio_activity", "error": "no speech"})).is_none());
        assert!(ev(json!({"message_type": "session_started", "session_id": "s"})).is_none());
        assert!(matches!(ev(json!({"message_type": "auth_error", "error": "bad key"})), Some(SttEvent::Error(_))));
    }
}

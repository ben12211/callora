//! Cartesia streaming speech-to-text (`ink-whisper`, multilingual, Hebrew included).
//!
//! Audio goes in as raw μ-law 8 kHz binary frames, exactly what Twilio delivers, so nothing
//! is transcoded. The API key travels only in the `X-API-Key` handshake header, never in
//! the URL, as in the legacy integration.
//!
//! Endpointing: the runtime's VAD decides when the caller stopped and sends `finalize`, so
//! Cartesia's own silence timeout is set longer than the VAD's. The legacy system learned
//! that a short service-side timeout split Hebrew sentences at every comma pause.

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use callora_runtime::ports::{SpeechToText, SttEvent, SttInput, SttSession};

pub const DEFAULT_BASE_URL: &str = "wss://api.cartesia.ai";
pub const DEFAULT_MODEL: &str = "ink-whisper";
pub const DEFAULT_VERSION: &str = "2026-03-01";

pub struct Cartesia {
    api_key: String,
    base_url: String,
    model: String,
    version: String,
    /// Service-side silence that ends an utterance when no `finalize` came first.
    max_silence_secs: f32,
}

impl Cartesia {
    pub fn new(api_key: String, base_url: Option<String>, model: Option<String>, version: Option<String>) -> Self {
        let nonblank = |v: Option<String>| v.filter(|s| !s.trim().is_empty());
        Self {
            api_key,
            base_url: nonblank(base_url).unwrap_or_else(|| DEFAULT_BASE_URL.into()).trim_end_matches('/').into(),
            model: nonblank(model).unwrap_or_else(|| DEFAULT_MODEL.into()),
            version: nonblank(version).unwrap_or_else(|| DEFAULT_VERSION.into()),
            max_silence_secs: 1.0,
        }
    }

    pub fn url(&self, language: &str) -> String {
        let lang = language.split('-').next().unwrap_or(language);
        let mut url = format!(
            "{}/stt/websocket?model={}&encoding=pcm_mulaw&sample_rate=8000&cartesia_version={}&language={lang}",
            self.base_url, self.model, self.version
        );
        if self.model.starts_with("ink-whisper") {
            url.push_str(&format!("&max_silence_duration_secs={}", self.max_silence_secs));
        }
        url
    }
}

#[async_trait]
impl SpeechToText for Cartesia {
    async fn open(&self, language: &str) -> anyhow::Result<SttSession> {
        let mut request = self.url(language).into_client_request()?;
        request.headers_mut().insert("X-API-Key", self.api_key.parse()?);
        let (ws, _) =
            tokio::time::timeout(std::time::Duration::from_secs(8), tokio_tungstenite::connect_async(request))
                .await??;
        let (mut sink, mut stream) = ws.split();
        let (in_tx, mut in_rx) = mpsc::channel::<SttInput>(512);
        let (ev_tx, ev_rx) = mpsc::channel::<SttEvent>(64);
        let (flush_tx, mut flush_rx) = mpsc::unbounded_channel::<()>();

        tokio::spawn(async move {
            while let Some(input) = in_rx.recv().await {
                let msg = match input {
                    SttInput::Audio(a) => Message::Binary(a),
                    SttInput::Finalize => {
                        let _ = flush_tx.send(());
                        Message::Text("finalize".into())
                    }
                    SttInput::Close => {
                        let _ = sink.send(Message::Text("done".into())).await;
                        break;
                    }
                };
                if sink.send(msg).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        tokio::spawn(async move {
            // Finals are deltas. After a `finalize`, collect them until `flush_done` so one
            // utterance becomes one transcript; otherwise each final is an utterance.
            let mut pending = String::new();
            let mut flushing = false;
            loop {
                tokio::select! {
                    Some(()) = flush_rx.recv() => flushing = true,
                    msg = stream.next() => {
                        let Some(Ok(msg)) = msg else { break };
                        let Message::Text(text) = msg else { continue };
                        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
                        match v.get("type").and_then(|t| t.as_str()) {
                            Some("transcript") => {
                                let t = v.get("text").and_then(|t| t.as_str()).unwrap_or("");
                                if v.get("is_final").and_then(serde_json::Value::as_bool) == Some(true) {
                                    pending.push_str(t);
                                    if !flushing && !pending.trim().is_empty() {
                                        let _ = ev_tx.send(SttEvent::Final(std::mem::take(&mut pending))).await;
                                    }
                                } else if !t.trim().is_empty() {
                                    let _ = ev_tx.send(SttEvent::Partial(t.to_string())).await;
                                }
                            }
                            Some("flush_done") => {
                                flushing = false;
                                if !pending.trim().is_empty() {
                                    let _ = ev_tx.send(SttEvent::Final(std::mem::take(&mut pending))).await;
                                }
                            }
                            Some("error") => {
                                let msg = v.get("message").or_else(|| v.get("error")).map(ToString::to_string).unwrap_or_default();
                                let _ = ev_tx.send(SttEvent::Error(msg)).await;
                            }
                            _ => {}
                        }
                    }
                }
            }
            let _ = ev_tx.send(SttEvent::Closed).await;
        });

        Ok(SttSession { input: in_tx, events: ev_rx })
    }

    fn name(&self) -> &'static str {
        "cartesia"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_carries_format_language_and_silence_but_not_the_key() {
        let c = Cartesia::new("secret-key".into(), None, None, None);
        let url = c.url("he-IL");
        assert!(url
            .starts_with("wss://api.cartesia.ai/stt/websocket?model=ink-whisper&encoding=pcm_mulaw&sample_rate=8000"));
        assert!(url.contains("language=he"));
        assert!(url.contains("max_silence_duration_secs=1"));
        assert!(!url.contains("secret-key"));
    }
}

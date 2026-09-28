//! Deepgram Nova-3 streaming speech-to-text, in Hebrew.
//!
//! Audio goes in as raw μ-law 8 kHz binary frames, exactly what Twilio delivers, so nothing
//! is transcoded. The API key travels only in the `Authorization: Token` handshake header,
//! never in the URL.
//!
//! Endpointing stays with the runtime's VAD (`endpointing=false`): `Finalize` asks Deepgram
//! to flush what it has heard, and the finals up to the one marked `from_finalize` are the
//! utterance. `keyterm` biases recognition toward the business's place and street names,
//! as Scribe's keyterms do.

use std::time::Duration;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use callora_runtime::ports::{SpeechToText, SttEvent, SttInput, SttSession};

pub const DEFAULT_BASE_URL: &str = "wss://api.deepgram.com";
pub const DEFAULT_MODEL: &str = "nova-3";
/// Keyterms share a budget of 500 tokens per session, and a session over it is refused,
/// which would leave a live call deaf. A token is at least one character, so counting
/// characters (and one per term for the separator) stays under it whatever the tokenizer
/// makes of Hebrew.
const KEYTERM_CHAR_BUDGET: usize = 450;
const MAX_KEYTERM_CHARS: usize = 50;
/// Deepgram closes a session that hears nothing for 10 s. A session opened ahead of time
/// (biased with a city's streets, waiting to take over) gets no audio until it does.
const KEEPALIVE: Duration = Duration::from_secs(4);
/// `from_finalize` is not guaranteed: a finalize whose results never say so ends after this.
const FINALIZE_WAIT: Duration = Duration::from_millis(1500);

const FINALIZE: &str = r#"{"type":"Finalize"}"#;
const KEEP_ALIVE: &str = r#"{"type":"KeepAlive"}"#;
const CLOSE_STREAM: &str = r#"{"type":"CloseStream"}"#;

pub struct Deepgram {
    api_key: String,
    base_url: String,
    model: String,
}

impl Deepgram {
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
        let mut url = url::Url::parse(&format!("{}/v1/listen", self.base_url))?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("model", &self.model)
                .append_pair("language", lang)
                .append_pair("encoding", "mulaw")
                .append_pair("sample_rate", "8000")
                .append_pair("channels", "1")
                .append_pair("interim_results", "true")
                .append_pair("endpointing", "false");
            // In the caller's order, so the city and its streets come before the business's words.
            let mut budget = KEYTERM_CHAR_BUDGET;
            for term in keyterms.iter().map(|t| t.trim()).filter(|t| !t.is_empty()) {
                let chars = term.chars().count();
                if chars > MAX_KEYTERM_CHARS {
                    continue;
                }
                if chars + 1 > budget {
                    break;
                }
                budget -= chars + 1;
                q.append_pair("keyterm", term);
            }
        }
        Ok(url.into())
    }
}

/// A server message, as far as the session cares.
#[derive(Debug, PartialEq)]
enum Heard {
    Result { text: String, is_final: bool, from_finalize: bool },
    Error(String),
    Other,
}

fn heard(v: &Value) -> Heard {
    let flag = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
    match v.get("type").and_then(Value::as_str) {
        Some("Results") => Heard::Result {
            text: v.pointer("/channel/alternatives/0/transcript").and_then(Value::as_str).unwrap_or("").trim().into(),
            is_final: flag("is_final"),
            from_finalize: flag("from_finalize"),
        },
        Some("Error") => Heard::Error(
            ["description", "message", "err_msg"]
                .iter()
                .find_map(|k| v.get(*k).and_then(Value::as_str))
                .unwrap_or("unknown error")
                .into(),
        ),
        _ => Heard::Other,
    }
}

/// The finals of the utterance in progress. Deepgram finalizes a long sentence in pieces on
/// its own; they are one utterance until the runtime's finalize ends it.
#[derive(Default)]
struct Utterance {
    finals: Vec<String>,
}

impl Utterance {
    fn push_final(&mut self, text: String) {
        if !text.is_empty() {
            self.finals.push(text);
        }
    }

    /// Everything heard so far, with the words not yet final after it.
    fn partial(&self, interim: &str) -> Option<String> {
        let text =
            self.finals.iter().map(String::as_str).chain([interim]).filter(|t| !t.is_empty()).collect::<Vec<_>>();
        (!text.is_empty()).then(|| text.join(" "))
    }

    fn take(&mut self) -> Option<String> {
        let text = std::mem::take(&mut self.finals).join(" ");
        (!text.is_empty()).then_some(text)
    }
}

#[async_trait]
impl SpeechToText for Deepgram {
    async fn open(&self, language: &str, keyterms: &[String]) -> anyhow::Result<SttSession> {
        let mut request = self.url(language, keyterms)?.into_client_request()?;
        request.headers_mut().insert("Authorization", format!("Token {}", self.api_key).parse()?);
        let (ws, _) = tokio::time::timeout(Duration::from_secs(8), tokio_tungstenite::connect_async(request)).await??;
        let (mut sink, mut stream) = ws.split();
        let (in_tx, mut in_rx) = mpsc::channel::<SttInput>(512);
        let (ev_tx, ev_rx) = mpsc::channel::<SttEvent>(64);
        let (finalize_tx, mut finalize_rx) = mpsc::unbounded_channel::<()>();

        tokio::spawn(async move {
            loop {
                let msg = match tokio::time::timeout(KEEPALIVE, in_rx.recv()).await {
                    Err(_) => Message::Text(KEEP_ALIVE.into()),
                    Ok(Some(SttInput::Audio(a))) => Message::Binary(a),
                    Ok(Some(SttInput::Finalize)) => {
                        let _ = finalize_tx.send(());
                        Message::Text(FINALIZE.into())
                    }
                    Ok(Some(SttInput::Close) | None) => {
                        let _ = sink.send(Message::Text(CLOSE_STREAM.into())).await;
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
            let mut utterance = Utterance::default();
            let mut deadline: Option<Instant> = None;
            loop {
                let overdue = async {
                    match deadline {
                        Some(d) => tokio::time::sleep_until(d).await,
                        None => std::future::pending().await,
                    }
                };
                let event = tokio::select! {
                    Some(()) = finalize_rx.recv() => {
                        deadline.get_or_insert(Instant::now() + FINALIZE_WAIT);
                        None
                    }
                    () = overdue => {
                        deadline = None;
                        utterance.take().map(SttEvent::Final)
                    }
                    msg = stream.next() => {
                        let Some(Ok(msg)) = msg else { break };
                        let Message::Text(text) = msg else { continue };
                        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
                        match heard(&v) {
                            Heard::Result { text, is_final: true, from_finalize } => {
                                utterance.push_final(text);
                                if from_finalize {
                                    deadline = None;
                                    utterance.take().map(SttEvent::Final)
                                } else {
                                    None
                                }
                            }
                            Heard::Result { text, .. } => utterance.partial(&text).map(SttEvent::Partial),
                            Heard::Error(e) => Some(SttEvent::Error(e)),
                            Heard::Other => None,
                        }
                    }
                };
                if let Some(e) = event {
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
        "deepgram"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn url_carries_model_language_format_and_keyterms_but_not_the_key() {
        let d = Deepgram::new("secret-key".into(), None, None);
        let terms = vec!["בני ברק".to_string(), " ".to_string(), "x".repeat(60), "רחוב הרצל".to_string()];
        let url = d.url("he-IL", &terms).unwrap();
        let pairs: Vec<(String, String)> = url::Url::parse(&url).unwrap().query_pairs().into_owned().collect();
        assert!(url.starts_with("wss://api.deepgram.com/v1/listen?"));
        for (k, v) in [
            ("model", "nova-3"),
            ("language", "he"),
            ("encoding", "mulaw"),
            ("sample_rate", "8000"),
            ("interim_results", "true"),
            ("endpointing", "false"),
        ] {
            assert!(pairs.contains(&(k.into(), v.into())), "{k}={v} in {url}");
        }
        let keyterms: Vec<&str> = pairs.iter().filter(|(k, _)| k == "keyterm").map(|(_, v)| v.as_str()).collect();
        assert_eq!(keyterms, vec!["בני ברק", "רחוב הרצל"], "blank and overlong terms are dropped");
        assert!(!url.contains("secret-key"));
    }

    #[test]
    fn keyterms_stop_at_the_budget_and_keep_their_order() {
        let terms: Vec<String> = (0..200).map(|n| format!("רחוב מספר {n:03}")).collect();
        let url = Deepgram::new("k".into(), None, None).url("he-IL", &terms).unwrap();
        let sent: Vec<String> = url::Url::parse(&url)
            .unwrap()
            .query_pairs()
            .filter(|(k, _)| k == "keyterm")
            .map(|(_, v)| v.into_owned())
            .collect();
        let chars: usize = sent.iter().map(|t| t.chars().count() + 1).sum();
        assert!(chars <= KEYTERM_CHAR_BUDGET);
        assert_eq!(sent.len(), KEYTERM_CHAR_BUDGET / 14);
        assert_eq!(sent.first().map(String::as_str), Some("רחוב מספר 000"));
    }

    #[test]
    fn server_messages() {
        let result = |text: &str, is_final: bool, from_finalize: bool| {
            json!({"type": "Results", "is_final": is_final, "from_finalize": from_finalize,
                   "channel": {"alternatives": [{"transcript": text, "confidence": 0.9}]}})
        };
        assert_eq!(
            heard(&result(" לתל אביב ", true, true)),
            Heard::Result { text: "לתל אביב".into(), is_final: true, from_finalize: true }
        );
        assert_eq!(
            heard(&json!({"type": "Results", "channel": {"alternatives": [{"transcript": "לתל"}]}})),
            Heard::Result { text: "לתל".into(), is_final: false, from_finalize: false }
        );
        assert_eq!(heard(&json!({"type": "Error", "description": "bad key"})), Heard::Error("bad key".into()));
        assert_eq!(heard(&json!({"type": "Metadata", "request_id": "r"})), Heard::Other);
    }

    #[test]
    fn finals_in_pieces_are_one_utterance() {
        let mut u = Utterance::default();
        assert_eq!(u.partial(""), None);
        u.push_final("צריך מונית".into());
        u.push_final(String::new());
        assert_eq!(u.partial("מהרצל").as_deref(), Some("צריך מונית מהרצל"));
        u.push_final("מהרצל 12".into());
        assert_eq!(u.take().as_deref(), Some("צריך מונית מהרצל 12"));
        assert_eq!(u.take(), None, "taken once");
    }
}

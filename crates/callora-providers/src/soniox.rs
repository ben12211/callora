//! Soniox real-time speech-to-text (`stt-rt-v5`), in Hebrew.
//!
//! Audio goes in as raw μ-law 8 kHz binary frames, exactly what Twilio delivers. The API key
//! travels only in the `Authorization` handshake header. The first message configures the
//! session: the model, the audio format, Hebrew as the only language, and the context (the
//! call's domain and the words expected: a city's streets while its street is asked).
//!
//! Endpointing stays with the runtime's VAD: `Finalize` sends `{"type":"finalize"}`, the
//! tokens heard so far come back final, and a `<fin>` token closes the utterance. Every token
//! carries a confidence, so the words it was unsure of reach the agent as with OpenAI.

use std::time::Duration;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use callora_runtime::ports::{SpeechToText, SttEvent, SttInput, SttSession};

pub const DEFAULT_URL: &str = "wss://stt-rt.soniox.com/transcribe-websocket";
pub const DEFAULT_MODEL: &str = "stt-rt-v5";
/// The context may hold 8,000 tokens; Hebrew takes more of them per character than English,
/// so the terms stop well short. A session refused for them opens without them.
const TERM_CHAR_BUDGET: usize = 4000;
const MAX_TERM_CHARS: usize = 50;
/// A session hearing nothing for a while is closed; one opened ahead of time (biased with a
/// city's streets, waiting to take over) gets no audio until it does.
const KEEPALIVE: Duration = Duration::from_secs(5);
/// `<fin>` is the end of a finalize; one that never comes ends the utterance after this.
const FINALIZE_WAIT: Duration = Duration::from_millis(1500);
/// A word heard with less confidence than this goes to the agent as unsure.
pub const UNSURE_BELOW: f32 = 0.8;

const FINALIZE: &str = r#"{"type":"finalize"}"#;
const KEEP_ALIVE: &str = r#"{"type":"keepalive"}"#;

pub struct Soniox {
    api_key: String,
    url: String,
    model: String,
}

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

impl Soniox {
    pub fn new(api_key: String, url: Option<String>, model: Option<String>) -> Self {
        let nonblank = |v: Option<String>| v.filter(|s| !s.trim().is_empty());
        Self {
            api_key,
            url: nonblank(url).unwrap_or_else(|| DEFAULT_URL.into()),
            model: nonblank(model).unwrap_or_else(|| DEFAULT_MODEL.into()),
        }
    }

    /// The first message of a session.
    pub fn config(&self, language: &str, keyterms: &[String]) -> Value {
        let lang = language.split('-').next().unwrap_or(language);
        let mut terms = Vec::new();
        let mut budget = TERM_CHAR_BUDGET;
        for term in keyterms.iter().map(|t| t.trim()).filter(|t| !t.is_empty()) {
            let chars = term.chars().count();
            if chars > MAX_TERM_CHARS {
                continue;
            }
            if chars + 1 > budget {
                break;
            }
            budget -= chars + 1;
            terms.push(term);
        }
        let mut context = json!({
            "general": [
                { "key": "domain", "value": "Taxi booking" },
                { "key": "setting", "value": "A phone call to an Israeli taxi company" },
                { "key": "topic", "value": "Pickup and destination addresses (towns, streets, house numbers), passengers, names" },
            ],
        });
        if !terms.is_empty() {
            context["terms"] = json!(terms);
        }
        json!({
            "model": self.model,
            "audio_format": "mulaw",
            "sample_rate": 8000,
            "num_channels": 1,
            "language_hints": [lang],
            "language_hints_strict": true,
            "enable_endpoint_detection": false,
            "context": context,
        })
    }

    async fn connect(&self, config: &Value) -> anyhow::Result<Socket> {
        let mut request = self.url.as_str().into_client_request()?;
        request.headers_mut().insert("Authorization", format!("Bearer {}", self.api_key).parse()?);
        let (mut ws, _) =
            tokio::time::timeout(Duration::from_secs(8), tokio_tungstenite::connect_async(request)).await??;
        ws.send(Message::Text(config.to_string().into())).await?;
        Ok(ws)
    }
}

/// A server message, as far as the session cares.
#[derive(Debug, PartialEq)]
enum Heard {
    /// Tokens: (text, final, confidence). `fin` when a finalize ended with them.
    Tokens {
        tokens: Vec<(String, bool, f32)>,
        fin: bool,
    },
    Error(String),
    Finished,
}

fn heard(v: &Value) -> Heard {
    if let Some(code) = v.get("error_code") {
        let kind = v.get("error_type").and_then(Value::as_str).unwrap_or("");
        let message = v.get("error_message").and_then(Value::as_str).unwrap_or("unknown error");
        return Heard::Error(format!("soniox {code} {kind}: {message}"));
    }
    if v.get("finished").and_then(Value::as_bool) == Some(true) {
        return Heard::Finished;
    }
    let mut fin = false;
    let tokens = v
        .get("tokens")
        .and_then(Value::as_array)
        .map(|ts| {
            ts.iter()
                .filter_map(|t| {
                    let text = t.get("text").and_then(Value::as_str)?;
                    if text == "<fin>" {
                        fin = true;
                        return None;
                    }
                    if text == "<end>" {
                        return None;
                    }
                    let is_final = t.get("is_final").and_then(Value::as_bool).unwrap_or(false);
                    let confidence = t.get("confidence").and_then(Value::as_f64).unwrap_or(1.0) as f32;
                    Some((text.to_string(), is_final, confidence))
                })
                .collect()
        })
        .unwrap_or_default();
    Heard::Tokens { tokens, fin }
}

/// The utterance in progress: its final tokens, and the latest non-final ones (each message
/// carries all the non-final tokens anew).
#[derive(Default)]
struct Utterance {
    finals: Vec<(String, f32)>,
    interim: String,
}

impl Utterance {
    fn push(&mut self, tokens: Vec<(String, bool, f32)>) {
        self.interim.clear();
        for (text, is_final, confidence) in tokens {
            if is_final {
                self.finals.push((text, confidence));
            } else {
                self.interim.push_str(&text);
            }
        }
    }

    fn text(&self) -> String {
        self.finals.iter().map(|(t, _)| t.as_str()).collect::<String>()
    }

    fn partial(&self) -> Option<String> {
        let text = format!("{}{}", self.text(), self.interim);
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_string())
    }

    /// The utterance's words and the ones it was unsure of; empty afterwards.
    fn take(&mut self) -> Option<(String, Vec<(String, f32)>)> {
        let text = self.text().trim().to_string();
        let unsure = unsure_words(&self.finals);
        self.finals.clear();
        self.interim.clear();
        (!text.is_empty()).then_some((text, unsure))
    }
}

/// Words (tokens joined at the spaces) with their least confident token, the unsure ones.
fn unsure_words(tokens: &[(String, f32)]) -> Vec<(String, f32)> {
    let mut words: Vec<(String, f32)> = Vec::new();
    let mut current = (String::new(), 1.0f32);
    for (token, confidence) in tokens {
        for (i, piece) in token.split(' ').enumerate() {
            if i > 0 && !current.0.is_empty() {
                words.push(std::mem::replace(&mut current, (String::new(), 1.0)));
            }
            if !piece.is_empty() {
                current.0.push_str(piece);
                current.1 = current.1.min(*confidence);
            }
        }
    }
    if !current.0.is_empty() {
        words.push(current);
    }
    words
        .into_iter()
        .map(|(w, p)| (w.trim_matches(|c: char| !c.is_alphanumeric()).to_string(), p))
        .filter(|(w, p)| !w.is_empty() && *p < UNSURE_BELOW)
        .collect()
}

#[async_trait]
impl SpeechToText for Soniox {
    async fn open(&self, language: &str, keyterms: &[String]) -> anyhow::Result<SttSession> {
        let ws = match self.connect(&self.config(language, keyterms)).await {
            Ok(ws) => ws,
            Err(e) if !keyterms.is_empty() && e.to_string().contains("400") => {
                tracing::warn!(error = %e, terms = keyterms.len(), "soniox refused the terms; listening without them");
                self.connect(&self.config(language, &[])).await?
            }
            Err(e) => return Err(e),
        };
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
                        // An empty text frame ends the stream.
                        let _ = sink.send(Message::Text(String::new().into())).await;
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
                let events: Vec<SttEvent> = tokio::select! {
                    Some(()) = finalize_rx.recv() => {
                        deadline.get_or_insert(Instant::now() + FINALIZE_WAIT);
                        Vec::new()
                    }
                    () = overdue => {
                        deadline = None;
                        finished(utterance.take())
                    }
                    msg = stream.next() => {
                        let Some(Ok(msg)) = msg else { break };
                        let Message::Text(text) = msg else { continue };
                        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
                        match heard(&v) {
                            Heard::Tokens { tokens, fin } => {
                                utterance.push(tokens);
                                if fin {
                                    deadline = None;
                                    finished(utterance.take())
                                } else {
                                    utterance.partial().map(SttEvent::Partial).into_iter().collect()
                                }
                            }
                            Heard::Error(e) => vec![SttEvent::Error(e)],
                            Heard::Finished => break,
                        }
                    }
                };
                for e in events {
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
        "soniox"
    }
}

/// An utterance's events: its unsure words first, then its words.
fn finished(taken: Option<(String, Vec<(String, f32)>)>) -> Vec<SttEvent> {
    let Some((text, unsure)) = taken else { return Vec::new() };
    let mut events = Vec::new();
    if !unsure.is_empty() {
        events.push(SttEvent::Unsure(unsure));
    }
    events.push(SttEvent::Final(text));
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_config_asks_for_hebrew_mulaw_and_the_terms_within_the_budget() {
        let s = Soniox::new("secret-key".into(), None, None);
        let terms: Vec<String> = ["בני ברק", " ", &"x".repeat(60), "רבי עקיבא"].iter().map(|t| t.to_string()).collect();
        let c = s.config("he-IL", &terms);
        assert_eq!(c["model"], "stt-rt-v5");
        assert_eq!(c["audio_format"], "mulaw");
        assert_eq!(c["sample_rate"], 8000);
        assert_eq!(c["language_hints"], json!(["he"]));
        assert_eq!(c["language_hints_strict"], true);
        assert_eq!(c["context"]["terms"], json!(["בני ברק", "רבי עקיבא"]), "blank and overlong terms are dropped");
        assert!(!c.to_string().contains("secret-key"), "the key is only in the header");
        let many: Vec<String> = (0..2000).map(|n| format!("רחוב מספר {n:04}")).collect();
        let sent = s.config("he-IL", &many)["context"]["terms"].as_array().unwrap().len();
        assert_eq!(sent, 4000 / 15, "the terms stop at the budget");
        assert!(s.config("he-IL", &[])["context"].get("terms").is_none());
    }

    #[test]
    fn server_messages() {
        let v = json!({ "tokens": [
            { "text": "בן", "is_final": true, "confidence": 0.95 },
            { "text": " זכאי", "is_final": false, "confidence": 0.6 },
            { "text": "<fin>", "is_final": true },
        ]});
        assert_eq!(
            heard(&v),
            Heard::Tokens { tokens: vec![("בן".into(), true, 0.95), (" זכאי".into(), false, 0.6)], fin: true }
        );
        assert_eq!(
            heard(
                &json!({ "tokens": [], "error_code": 402, "error_type": "payment_required", "error_message": "Balance exhausted" })
            ),
            Heard::Error("soniox 402 payment_required: Balance exhausted".into())
        );
        assert_eq!(heard(&json!({ "tokens": [], "finished": true })), Heard::Finished);
    }

    #[test]
    fn an_utterance_is_its_final_tokens_and_the_unsure_words_among_them() {
        let mut u = Utterance::default();
        u.push(vec![("צריך".into(), true, 0.99), (" מונ".into(), false, 0.9)]);
        assert_eq!(u.partial().as_deref(), Some("צריך מונ"));
        u.push(vec![
            (" מונית".into(), true, 0.97),
            (" מבן".into(), true, 0.9),
            (" זכ".into(), true, 0.5),
            ("אי".into(), true, 0.9),
        ]);
        u.push(vec![(" 45".into(), false, 0.9)]);
        assert_eq!(u.partial().as_deref(), Some("צריך מונית מבן זכאי 45"), "the non-final tokens are replaced");
        let (text, unsure) = u.take().unwrap();
        assert_eq!(text, "צריך מונית מבן זכאי", "only final tokens make the utterance");
        assert_eq!(unsure.len(), 1);
        assert_eq!(unsure[0].0, "זכאי");
        assert!((unsure[0].1 - 0.5).abs() < 1e-6);
        assert_eq!(u.take(), None, "taken once");
        assert_eq!(finished(Some(("כן".into(), vec![]))), vec![SttEvent::Final("כן".into())]);
    }
}

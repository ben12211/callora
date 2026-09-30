//! OpenAI realtime transcription (`gpt-transcribe` by default) over a WebSocket.
//!
//! On 186 recorded utterances of live Hebrew calls, it got the places and names that the
//! Scribe stream garbled ("מירושלים" heard as "אור שדלי", "בנייני האומה" as "בניין האומה
//! יגוזע להם"). It is given a short description of the call, and no keyterms: a list of
//! place names made the recognizers write places the caller never said ("לא" as "לוד").
//!
//! Endpointing stays with the runtime's VAD: audio is appended as it arrives, `Finalize`
//! commits the buffer, and the committed turn's transcript is the utterance. Audio goes in
//! as 16-bit PCM at 24 kHz, the rate the service takes, upsampled from Twilio's μ-law
//! 8 kHz. The API key travels only in the `Authorization` handshake header.

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

use callora_runtime::ports::{SpeechToText, SttEvent, SttInput, SttSession};

pub const DEFAULT_URL: &str = "wss://api.openai.com/v1/realtime?intent=transcription";
pub const DEFAULT_MODEL: &str = "gpt-transcribe";
pub const DEFAULT_PROMPT: &str = "A phone call in Hebrew to a business in Israel. Callers give cities, streets and \
                                  house numbers, numbers of people, and names. Write everything in Hebrew letters, \
                                  names and English words too: דוד, ביי, אוקיי.";

/// Hints at most: the prompt is context, not a list the recognizer must pick from.
const MAX_HINTS: usize = 40;

pub struct OpenAiStt {
    api_key: String,
    url: String,
    model: String,
    prompt: String,
    /// The words the call expects (keyterms) go into the prompt. Off by default: a place
    /// list made another recognizer write places nobody said.
    hints: bool,
}

impl OpenAiStt {
    pub fn new(api_key: String, url: Option<String>, model: Option<String>, prompt: Option<String>) -> Self {
        let nonblank = |v: Option<String>| v.filter(|s| !s.trim().is_empty());
        Self {
            api_key,
            url: nonblank(url).unwrap_or_else(|| DEFAULT_URL.into()),
            model: nonblank(model).unwrap_or_else(|| DEFAULT_MODEL.into()),
            prompt: nonblank(prompt).unwrap_or_else(|| DEFAULT_PROMPT.into()),
            hints: false,
        }
    }

    pub fn with_hints(mut self, hints: bool) -> Self {
        self.hints = hints;
        self
    }

    /// The prompt, with the words the call expects when hints are on: "בן זכאי" in אלעד was
    /// written "בן זה קיץ" and "באיזה קו" without them.
    fn prompt_for(&self, keyterms: &[String]) -> String {
        let terms: Vec<&str> = keyterms.iter().map(|t| t.trim()).filter(|t| !t.is_empty()).take(MAX_HINTS).collect();
        if !self.hints || terms.is_empty() {
            return self.prompt.clone();
        }
        format!("{} Words that may come up: {}.", self.prompt, terms.join(", "))
    }

    pub fn session_update(&self, language: &str, keyterms: &[String]) -> Value {
        let lang = language.split('-').next().unwrap_or(language);
        let prompt = self.prompt_for(keyterms);
        json!({
            "type": "session.update",
            "session": {
                "type": "transcription",
                "audio": {
                    "input": {
                        "format": { "type": "audio/pcm", "rate": 24000 },
                        "transcription": { "model": self.model, "prompt": prompt, "languages": [lang] },
                        "turn_detection": null
                    }
                }
            }
        })
    }
}

/// μ-law 8 kHz → 16-bit little-endian PCM at 24 kHz (each sample, then two steps toward the
/// next).
pub fn pcm24k(mulaw: &[u8]) -> Vec<u8> {
    let samples: Vec<i32> = mulaw.iter().map(|&b| i32::from(callora_audio::mulaw::decode(b))).collect();
    let mut out = Vec::with_capacity(samples.len() * 6);
    for (i, &s) in samples.iter().enumerate() {
        let next = samples.get(i + 1).copied().unwrap_or(s);
        for step in 0..3 {
            let v = (s + (next - s) * step / 3) as i16;
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    out
}

fn append(audio: &[u8]) -> String {
    json!({ "type": "input_audio_buffer.append", "audio": STANDARD.encode(pcm24k(audio)) }).to_string()
}

/// What a server message means for the session. Partial text accumulates per turn.
fn event(v: &Value, partial: &mut String) -> Option<SttEvent> {
    match v.get("type").and_then(Value::as_str)? {
        "conversation.item.input_audio_transcription.delta" => {
            partial.push_str(v.get("delta").and_then(Value::as_str).unwrap_or(""));
            Some(SttEvent::Partial(partial.trim().to_string()))
                .filter(|e| !matches!(e, SttEvent::Partial(t) if t.is_empty()))
        }
        "conversation.item.input_audio_transcription.completed" => {
            partial.clear();
            let text = v.get("transcript").and_then(Value::as_str).unwrap_or("").trim().to_string();
            Some(SttEvent::Final(text)).filter(|e| !matches!(e, SttEvent::Final(t) if t.is_empty()))
        }
        "conversation.item.input_audio_transcription.failed" => {
            partial.clear();
            Some(SttEvent::Error(format!(
                "transcription failed: {}",
                v.get("error").map(ToString::to_string).unwrap_or_default()
            )))
        }
        "error" => {
            let code = v.pointer("/error/code").and_then(Value::as_str).unwrap_or("");
            // A commit with nothing in it (the VAD fired on a click): nothing was said.
            if code.contains("buffer_too_small") || code.contains("commit_empty") {
                return None;
            }
            Some(SttEvent::Error(format!(
                "{code}: {}",
                v.pointer("/error/message").and_then(Value::as_str).unwrap_or("")
            )))
        }
        _ => None,
    }
}

#[async_trait]
impl SpeechToText for OpenAiStt {
    async fn open(&self, language: &str, keyterms: &[String]) -> anyhow::Result<SttSession> {
        let mut request = self.url.as_str().into_client_request()?;
        request.headers_mut().insert("Authorization", format!("Bearer {}", self.api_key).parse()?);
        let (ws, _) =
            tokio::time::timeout(std::time::Duration::from_secs(8), tokio_tungstenite::connect_async(request))
                .await??;
        let (mut sink, mut stream) = ws.split();
        sink.send(Message::Text(self.session_update(language, keyterms).to_string().into())).await?;
        let (in_tx, mut in_rx) = mpsc::channel::<SttInput>(512);
        let (ev_tx, ev_rx) = mpsc::channel::<SttEvent>(64);

        tokio::spawn(async move {
            while let Some(input) = in_rx.recv().await {
                let msg = match input {
                    SttInput::Audio(a) => append(&a),
                    SttInput::Finalize => json!({ "type": "input_audio_buffer.commit" }).to_string(),
                    SttInput::Close => break,
                };
                if sink.send(Message::Text(msg.into())).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        tokio::spawn(async move {
            let mut partial = String::new();
            while let Some(Ok(msg)) = stream.next().await {
                let Message::Text(text) = msg else { continue };
                let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
                if let Some(e) = event(&v, &mut partial) {
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
        "openai"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_session_asks_for_hebrew_pcm_and_manual_turns() {
        let s = OpenAiStt::new("k".into(), None, None, None);
        let u = s.session_update("he-IL", &["בן זכאי".into()]);
        assert_eq!(u["session"]["type"], "transcription");
        let input = &u["session"]["audio"]["input"];
        assert_eq!(input["format"], json!({ "type": "audio/pcm", "rate": 24000 }));
        assert_eq!(input["transcription"]["model"], "gpt-transcribe");
        assert_eq!(input["transcription"]["languages"], json!(["he"]));
        assert!(input["turn_detection"].is_null(), "the runtime's VAD ends the turn");
        assert!(input["transcription"].get("keywords").is_none(), "no place lists: they invent places");
        assert!(
            !input["transcription"]["prompt"].as_str().unwrap_or("").contains("בן זכאי"),
            "hints are off by default"
        );
    }

    #[test]
    fn hints_go_into_the_prompt_when_on() {
        let s = OpenAiStt::new("k".into(), None, None, None).with_hints(true);
        let terms: Vec<String> = (0..60).map(|i| format!("רחוב {i}")).collect();
        let u = s.session_update("he-IL", &terms);
        let prompt = u["session"]["audio"]["input"]["transcription"]["prompt"].as_str().unwrap_or("").to_string();
        assert!(prompt.starts_with(DEFAULT_PROMPT) && prompt.contains("רחוב 0, רחוב 1"), "{prompt}");
        assert!(prompt.contains("רחוב 39") && !prompt.contains("רחוב 40"), "forty at most");
        assert_eq!(
            s.session_update("he-IL", &[])["session"]["audio"]["input"]["transcription"]["prompt"],
            DEFAULT_PROMPT
        );
    }

    #[test]
    fn eight_kilohertz_becomes_twenty_four() {
        let pcm = pcm24k(&[0xFF, 0xFF]);
        assert_eq!(pcm.len(), 2 * 3 * 2, "three 16-bit samples for each μ-law byte");
        assert!(pcm.iter().all(|b| *b == 0), "silence stays silence");
    }

    #[test]
    fn deltas_build_the_partial_and_the_completed_turn_is_final() {
        let mut p = String::new();
        let d = |t: &str| json!({ "type": "conversation.item.input_audio_transcription.delta", "delta": t });
        assert!(matches!(event(&d("מירו"), &mut p), Some(SttEvent::Partial(t)) if t == "מירו"));
        assert!(matches!(event(&d("שלים"), &mut p), Some(SttEvent::Partial(t)) if t == "מירושלים"));
        let done =
            json!({ "type": "conversation.item.input_audio_transcription.completed", "transcript": " מירושלים. " });
        assert!(matches!(event(&done, &mut p), Some(SttEvent::Final(t)) if t == "מירושלים."));
        assert!(p.is_empty(), "the next turn starts clean");
        let empty = json!({ "type": "error", "error": { "code": "input_audio_buffer_commit_empty", "message": "" } });
        assert!(event(&empty, &mut p).is_none(), "a commit of nothing is not an error");
        let bad = json!({ "type": "error", "error": { "code": "invalid_api_key", "message": "no" } });
        assert!(matches!(event(&bad, &mut p), Some(SttEvent::Error(_))));
    }
}

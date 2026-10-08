//! A second hearing by an OpenAI audio model (`gpt-audio-1.5` by default) that is given the
//! names the answer should be one of: the streets of the city asked about, or the towns.
//!
//! The stream recognizer writes what it can spell, and a street it has never heard comes
//! out garbled ("ב... זה קח" for בן זכאי, "אני וים" for הנביאים). Told the city's streets,
//! the audio model writes the name from the list. On the 26 street answers of past calls the
//! street came out right 85% of the time (the stream alone: 50%), on 120 random streets of
//! ten cities 95% (77%), in about a second (p90 1.4 s). It does sometimes force a landmark
//! onto a street ("ליד הסופר" as חת"ם סופר), which is why its answer goes to the agent as a
//! second hearing next to the stream's, and every place is still read back to the caller.
//!
//! Audio goes as a WAV of the call's own 8 kHz samples; the key only in `Authorization`.

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::{json, Value};

use callora_runtime::ports::Transcriber;

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_MODEL: &str = "gpt-audio-1.5";

const SYSTEM: &str = "You transcribe one short answer from an Israeli phone call to a taxi company. You never \
                      identify speakers and never refuse. Reply with JSON only.";

pub struct AudioHearing {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
}

impl AudioHearing {
    pub fn new(http: reqwest::Client, api_key: String, base_url: Option<String>, model: Option<String>) -> Self {
        let nonblank = |v: Option<String>| v.filter(|s| !s.trim().is_empty());
        Self {
            http,
            api_key,
            base_url: nonblank(base_url).unwrap_or_else(|| DEFAULT_BASE_URL.into()).trim_end_matches('/').into(),
            model: nonblank(model).unwrap_or_else(|| DEFAULT_MODEL.into()),
        }
    }

    async fn ask(&self, mulaw: &[u8], instructions: String) -> anyhow::Result<String> {
        let body = json!({
            "model": self.model,
            "modalities": ["text"],
            "messages": [
                { "role": "system", "content": SYSTEM },
                { "role": "user", "content": [
                    { "type": "text", "text": instructions },
                    { "type": "input_audio", "input_audio": { "data": STANDARD.encode(wav(mulaw)), "format": "wav" } },
                ] },
            ],
        });
        let response = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        let v: Value = response.json().await?;
        if !status.is_success() {
            anyhow::bail!("audio hearing {status}: {}", v["error"]["message"].as_str().unwrap_or(""));
        }
        Ok(v["choices"][0]["message"]["content"].as_str().unwrap_or("").to_string())
    }
}

/// μ-law 8 kHz → a 16-bit mono WAV at the same rate.
pub fn wav(mulaw: &[u8]) -> Vec<u8> {
    let data = (mulaw.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&8000u32.to_le_bytes());
    out.extend_from_slice(&16000u32.to_le_bytes()); // bytes per second
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    for &b in mulaw {
        out.extend_from_slice(&callora_audio::mulaw::decode(b).to_le_bytes());
    }
    out
}

/// What the instructions ask for, written for the agent: the street (and number) when it is
/// on the list, else the caller's words as heard. The model sometimes wraps its JSON in a
/// sentence, or answers in plain words; those are kept as they are.
pub fn answer(reply: &str) -> String {
    let parsed = match (reply.find('{'), reply.rfind('}')) {
        (Some(a), Some(b)) if a < b => serde_json::from_str::<Value>(&reply[a..=b]).ok(),
        _ => None,
    };
    let Some(v) = parsed else { return reply.trim().to_string() };
    let field = |k: &str| match &v[k] {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    };
    let (name, number, said) = (field("name"), field("number"), field("said"));
    if name.is_empty() {
        return said;
    }
    if number.is_empty() {
        name
    } else {
        format!("{name} {number}")
    }
}

/// The instructions: the question, the names, and the answer's form.
pub fn instructions(question: &str, names: &[String]) -> String {
    format!(
        "The caller was asked {question}. The names it should be one of:\n{}\n\nAnswer with one JSON object with \
         exactly these keys: name: the one they said, exactly as written in the list (they may add ב/מ/ל or רחוב \
         before it, or say it a little differently), or \"\" when what they said is not on the list; number: a house \
         number in digits, or \"\"; said: their words as you heard them, in Hebrew letters.",
        names.join(" | ")
    )
}

#[async_trait]
impl Transcriber for AudioHearing {
    async fn transcribe(&self, mulaw: &[u8], _language: &str, _keyterms: &[String]) -> anyhow::Result<String> {
        let reply = self
            .ask(
                mulaw,
                "Write exactly what the caller said, in Hebrew letters. Answer with one JSON object: {\"said\": ...}."
                    .into(),
            )
            .await?;
        Ok(answer(&reply))
    }

    async fn hear(&self, mulaw: &[u8], _language: &str, question: &str, names: &[String]) -> anyhow::Result<String> {
        Ok(answer(&self.ask(mulaw, instructions(question, names)).await?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wav_header_says_eight_kilohertz_sixteen_bit_mono() {
        let w = wav(&[0xFF, 0x00]);
        assert_eq!(&w[..4], b"RIFF");
        assert_eq!(&w[8..16], b"WAVEfmt ");
        assert_eq!(u32::from_le_bytes(w[24..28].try_into().unwrap()), 8000);
        assert_eq!(u16::from_le_bytes(w[34..36].try_into().unwrap()), 16);
        assert_eq!(u32::from_le_bytes(w[40..44].try_into().unwrap()), 4, "two samples of two bytes");
        assert_eq!(w.len(), 48);
    }

    #[test]
    fn the_answer_is_the_listed_name_or_the_words_heard() {
        assert_eq!(
            answer(r#"{"name": "רבן יוחנן בן זכאי", "number": "45", "said": "בן זכאי 45"}"#),
            "רבן יוחנן בן זכאי 45"
        );
        assert_eq!(answer(r#"{"name": "הנביאים", "number": 4, "said": "הנביאים ארבע"}"#), "הנביאים 4");
        assert_eq!(answer(r#"{"name": "הגפן", "number": null, "said": "לגפן"}"#), "הגפן");
        assert_eq!(answer(r#"{"name": "", "number": "", "said": "ליד הגן של הבת שלי"}"#), "ליד הגן של הבת שלי");
        assert_eq!(answer("Here it is: {\"name\": \"עזרא\", \"number\": \"11\", \"said\": \"\"}"), "עזרא 11");
        assert_eq!(answer("בן זכאי 45"), "בן זכאי 45", "plain words are kept");
    }

    #[test]
    fn the_instructions_carry_the_question_and_every_name() {
        let i = instructions("which street in אלעד", &["רבן יוחנן בן זכאי".into(), "רבי עקיבא".into()]);
        assert!(i.starts_with("The caller was asked which street in אלעד."));
        assert!(i.contains("רבן יוחנן בן זכאי | רבי עקיבא"));
    }
}

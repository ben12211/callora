//! OpenAI-compatible chat completions with strict JSON-schema output, used only for
//! structured understanding (never to talk to the caller). `TEXT_LLM_BASE_URL` points it
//! at any compatible endpoint, as in the legacy deployment; [`OpenAi::gemini`] uses
//! Gemini's compatible endpoint.

use std::collections::VecDeque;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};

use callora_core::llm::LlmRequest;
use callora_runtime::ports::{LanguageModel, TextStream};

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_MODEL: &str = "gpt-4o-mini";
pub const GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta/openai";
pub const GEMINI_MODEL: &str = "gemini-3.8-flash";

pub struct OpenAi {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
    /// `reasoning_effort`, for models that think before answering.
    reasoning_effort: Option<String>,
    /// Gemini 3 models degrade below their default temperature, so it is not always sent.
    temperature: Option<f32>,
    /// Which provider this is, for logs and errors.
    label: &'static str,
}

impl OpenAi {
    pub fn new(http: reqwest::Client, api_key: String, base_url: Option<String>, model: Option<String>) -> Self {
        let nonblank = |v: Option<String>| v.filter(|s| !s.trim().is_empty());
        Self {
            http,
            api_key,
            base_url: nonblank(base_url).unwrap_or_else(|| DEFAULT_BASE_URL.into()).trim_end_matches('/').into(),
            model: nonblank(model).unwrap_or_else(|| DEFAULT_MODEL.into()),
            reasoning_effort: None,
            temperature: Some(0.0),
            label: "openai",
        }
    }

    /// Gemini through its OpenAI-compatible endpoint. Thinking is kept low unless
    /// `reasoning_effort` says otherwise: understanding sits on the reply path, and
    /// gemini-3.8-flash rejects "minimal".
    pub fn gemini(
        http: reqwest::Client,
        api_key: String,
        base_url: Option<String>,
        model: Option<String>,
        reasoning_effort: Option<String>,
    ) -> Self {
        let nonblank = |v: Option<String>| v.filter(|s| !s.trim().is_empty());
        Self {
            reasoning_effort: Some(nonblank(reasoning_effort).unwrap_or_else(|| "low".into())),
            temperature: None,
            label: "gemini",
            ..Self::new(
                http,
                api_key,
                Some(nonblank(base_url).unwrap_or_else(|| GEMINI_BASE_URL.into())),
                Some(nonblank(model).unwrap_or_else(|| GEMINI_MODEL.into())),
            )
        }
    }

    pub fn body(&self, request: &LlmRequest) -> Value {
        let mut body = json!({
            "model": self.model,
            "max_tokens": 400,
            "messages": [
                { "role": "system", "content": request.system },
                { "role": "user", "content": request.user },
            ],
            "response_format": {
                "type": "json_schema",
                "json_schema": { "name": "understanding", "strict": true, "schema": request.schema },
            },
        });
        if let Some(t) = self.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(effort) = &self.reasoning_effort {
            body["reasoning_effort"] = json!(effort);
        }
        body
    }
}

fn error_message(body: &Value) -> &str {
    // OpenAI sends {"error": ...}; Gemini's compatible endpoint wraps it in an array.
    body.pointer("/error/message")
        .or_else(|| body.pointer("/0/error/message"))
        .and_then(Value::as_str)
        .unwrap_or("unknown error")
}

/// Content deltas out of an OpenAI-style server-sent-event body.
fn sse_deltas(bytes: impl futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static) -> TextStream {
    let state = (Box::pin(bytes), String::new(), VecDeque::<String>::new(), false);
    futures::stream::unfold(state, |(mut bytes, mut buf, mut ready, mut done)| async move {
        loop {
            if let Some(delta) = ready.pop_front() {
                return Some((Ok(delta), (bytes, buf, ready, done)));
            }
            if done {
                return None;
            }
            match bytes.next().await {
                Some(Ok(chunk)) => {
                    buf.push_str(&String::from_utf8_lossy(&chunk));
                    while let Some(nl) = buf.find('\n') {
                        let line: String = buf.drain(..=nl).collect();
                        let Some(data) = line.trim().strip_prefix("data:") else { continue };
                        let data = data.trim();
                        if data == "[DONE]" {
                            done = true;
                            break;
                        }
                        if let Ok(v) = serde_json::from_str::<Value>(data) {
                            if let Some(d) = v.pointer("/choices/0/delta/content").and_then(Value::as_str) {
                                if !d.is_empty() {
                                    ready.push_back(d.to_string());
                                }
                            }
                        }
                    }
                }
                Some(Err(e)) => return Some((Err(e.into()), (bytes, buf, ready, true))),
                None => done = true,
            }
        }
    })
    .boxed()
}

#[async_trait]
impl LanguageModel for OpenAi {
    async fn stream(&self, request: &LlmRequest) -> anyhow::Result<TextStream> {
        let mut body = self.body(request);
        body["stream"] = json!(true);
        let resp = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            anyhow::bail!("LLM request failed with HTTP {status}: {}", error_message(&body));
        }
        Ok(sse_deltas(resp.bytes_stream()))
    }

    async fn warm(&self) {
        let _ = self
            .http
            .get(format!("{}/models", self.base_url))
            .bearer_auth(&self.api_key)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await;
    }

    async fn extract(&self, request: &LlmRequest) -> anyhow::Result<Value> {
        let resp = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&self.body(request))
            .send()
            .await?;
        let status = resp.status();
        let body: Value = resp.json().await?;
        if !status.is_success() {
            anyhow::bail!("LLM request failed with HTTP {status}: {}", error_message(&body));
        }
        let content = body
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("LLM reply has no content"))?;
        Ok(serde_json::from_str(content)?)
    }

    fn name(&self) -> &'static str {
        self.label
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> LlmRequest {
        LlmRequest { system: "s".into(), user: "u".into(), schema: json!({ "type": "object" }) }
    }

    #[test]
    fn gemini_uses_its_endpoint_low_thinking_and_default_temperature() {
        let llm = OpenAi::gemini(reqwest::Client::new(), "k".into(), None, None, None);
        assert_eq!(llm.base_url, GEMINI_BASE_URL);
        let body = llm.body(&request());
        assert_eq!(body["model"], "gemini-3.8-flash");
        assert_eq!(body["reasoning_effort"], "low");
        assert!(body.get("temperature").is_none());
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
    }

    #[tokio::test]
    async fn server_sent_events_become_content_deltas() {
        let events = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n\
                      data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"say\"}}]}\n\ndata: {\"choi";
        let tail = "ces\":[{\"delta\":{\"content\":\"\\\": 1}\"}}]}\n\ndata: [DONE]\n\n";
        let chunks: Vec<reqwest::Result<bytes::Bytes>> = vec![Ok(events.into()), Ok(tail.into())];
        let deltas: Vec<String> = sse_deltas(futures::stream::iter(chunks)).map(|d| d.unwrap()).collect().await;
        assert_eq!(deltas.concat(), "{\"say\": 1}");
    }

    #[test]
    fn openai_keeps_temperature_zero_and_sends_no_reasoning_effort() {
        let body = OpenAi::new(reqwest::Client::new(), "k".into(), None, None).body(&request());
        assert_eq!(body["model"], DEFAULT_MODEL);
        assert_eq!(body["temperature"], 0.0);
        assert!(body.get("reasoning_effort").is_none());
    }
}

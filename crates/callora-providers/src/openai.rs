//! OpenAI-compatible chat completions with strict JSON-schema output: the conversation
//! agent ([`OpenAi::agent`]) and structured understanding ([`OpenAi::new`]).
//! `TEXT_LLM_BASE_URL` points it at any compatible endpoint, as in the legacy deployment;
//! [`OpenAi::gemini`] uses Gemini's compatible endpoint.

use std::collections::VecDeque;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::oneshot;

use callora_core::llm::LlmRequest;
use callora_runtime::ports::{LanguageModel, TextStream, Usage, UsageReceiver};

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_MODEL: &str = "gpt-4o-mini";
pub const GEMINI_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta/openai";
pub const GEMINI_MODEL: &str = "gemini-3.8-flash";

/// The agent's model when `AGENT_MODEL` is unset: GPT-6 Sol, the reasoning-capable
/// successor of gpt-4o, with reasoning off so first words stay fast.
pub const AGENT_MODEL: &str = "gpt-6-sol";
/// The hedge behind it: GPT-6 Luna, OpenAI's low-latency model of the same family.
pub const AGENT_BACKUP_MODEL: &str = "gpt-6-luna";
/// No reasoning before the first word: a phone turn cannot wait for it. `low` is the
/// next step up when the eval shows it is worth the time.
pub const AGENT_REASONING_EFFORT: &str = "none";

/// Models that think before answering (the o-series and GPT-5 onwards). They take
/// `reasoning_effort` and `max_completion_tokens`, and reject a temperature.
pub fn is_reasoning_model(model: &str) -> bool {
    let m = model.trim().to_ascii_lowercase();
    let family = |prefix: &str| {
        m.strip_prefix(prefix).and_then(|rest| rest.chars().next()).is_some_and(|c| c.is_ascii_digit() && c >= '5')
    };
    family("gpt-") || ["o1", "o3", "o4"].iter().any(|p| m.starts_with(p))
}

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
        let model = nonblank(model).unwrap_or_else(|| DEFAULT_MODEL.into());
        Self {
            http,
            api_key,
            base_url: nonblank(base_url).unwrap_or_else(|| DEFAULT_BASE_URL.into()).trim_end_matches('/').into(),
            // Reasoning models only take their default temperature.
            temperature: (!is_reasoning_model(&model)).then_some(0.0),
            model,
            reasoning_effort: None,
            label: "openai",
        }
    }

    /// The conversation agent's model. A reasoning model gets `reasoning_effort` (default
    /// [`AGENT_REASONING_EFFORT`]); older models ignore it.
    pub fn agent(
        http: reqwest::Client,
        api_key: String,
        base_url: Option<String>,
        model: Option<String>,
        reasoning_effort: Option<String>,
    ) -> Self {
        let nonblank = |v: Option<String>| v.filter(|s| !s.trim().is_empty());
        let mut llm = Self::new(http, api_key, base_url, Some(nonblank(model).unwrap_or_else(|| AGENT_MODEL.into())));
        if is_reasoning_model(&llm.model) {
            llm.reasoning_effort = Some(nonblank(reasoning_effort).unwrap_or_else(|| AGENT_REASONING_EFFORT.into()));
        }
        llm
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

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn body(&self, request: &LlmRequest) -> Value {
        let mut body = json!({
            "model": self.model,
            "messages": [
                { "role": "system", "content": request.system },
                { "role": "user", "content": request.user },
            ],
            "response_format": {
                "type": "json_schema",
                "json_schema": { "name": "understanding", "strict": true, "schema": request.schema },
            },
        });
        if is_reasoning_model(&self.model) {
            // The limit counts reasoning tokens too: leave room for them when thinking is on.
            let thinking = self.reasoning_effort.as_deref().is_some_and(|e| e != "none" && e != "minimal");
            body["max_completion_tokens"] = json!(if thinking { 4000 } else { 600 });
        } else {
            body["max_tokens"] = json!(400);
        }
        if let Some(t) = self.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(effort) = &self.reasoning_effort {
            body["reasoning_effort"] = json!(effort);
        }
        body
    }

    async fn open_stream(&self, request: &LlmRequest, metered: bool) -> anyhow::Result<reqwest::Response> {
        let mut body = self.body(request);
        body["stream"] = json!(true);
        if metered && self.label == "openai" {
            // The last event then carries the reply's token counts.
            body["stream_options"] = json!({ "include_usage": true });
        }
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
            anyhow::bail!("{} ({}): HTTP {status}: {}", self.label, self.model, error_message(&body));
        }
        Ok(resp)
    }
}

fn error_message(body: &Value) -> &str {
    // OpenAI sends {"error": ...}; Gemini's compatible endpoint wraps it in an array.
    body.pointer("/error/message")
        .or_else(|| body.pointer("/0/error/message"))
        .and_then(Value::as_str)
        .unwrap_or("unknown error")
}

fn usage_of(v: &Value, model: &str) -> Option<Usage> {
    let u = v.get("usage").filter(|u| u.is_object())?;
    let n = |p: &str| u.pointer(p).and_then(Value::as_u64).unwrap_or(0);
    Some(Usage {
        model: v.get("model").and_then(Value::as_str).unwrap_or(model).to_string(),
        input: n("/prompt_tokens"),
        cached: n("/prompt_tokens_details/cached_tokens"),
        output: n("/completion_tokens"),
    })
}

/// Content deltas out of an OpenAI-style server-sent-event body; the token counts, when an
/// event carries them, go to `usage`.
fn sse_deltas(
    bytes: impl futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send + 'static,
    model: String,
    usage: Option<oneshot::Sender<Usage>>,
) -> TextStream {
    struct State<B> {
        bytes: std::pin::Pin<Box<B>>,
        buf: String,
        ready: VecDeque<String>,
        done: bool,
        model: String,
        usage: Option<oneshot::Sender<Usage>>,
    }
    let state = State { bytes: Box::pin(bytes), buf: String::new(), ready: VecDeque::new(), done: false, model, usage };
    futures::stream::unfold(state, |mut s| async move {
        loop {
            if let Some(delta) = s.ready.pop_front() {
                return Some((Ok(delta), s));
            }
            if s.done {
                return None;
            }
            match s.bytes.next().await {
                Some(Ok(chunk)) => {
                    s.buf.push_str(&String::from_utf8_lossy(&chunk));
                    while let Some(nl) = s.buf.find('\n') {
                        let line: String = s.buf.drain(..=nl).collect();
                        let Some(data) = line.trim().strip_prefix("data:") else { continue };
                        let data = data.trim();
                        if data == "[DONE]" {
                            s.done = true;
                            break;
                        }
                        if let Ok(v) = serde_json::from_str::<Value>(data) {
                            if let Some(d) = v.pointer("/choices/0/delta/content").and_then(Value::as_str) {
                                if !d.is_empty() {
                                    s.ready.push_back(d.to_string());
                                }
                            }
                            if let Some(u) = usage_of(&v, &s.model) {
                                if let Some(tx) = s.usage.take() {
                                    let _ = tx.send(u);
                                }
                            }
                        }
                    }
                }
                Some(Err(e)) => {
                    s.done = true;
                    return Some((Err(e.into()), s));
                }
                None => s.done = true,
            }
        }
    })
    .boxed()
}

#[async_trait]
impl LanguageModel for OpenAi {
    async fn stream(&self, request: &LlmRequest) -> anyhow::Result<TextStream> {
        let resp = self.open_stream(request, false).await?;
        Ok(sse_deltas(resp.bytes_stream(), self.model.clone(), None))
    }

    async fn stream_metered(&self, request: &LlmRequest) -> anyhow::Result<(TextStream, UsageReceiver)> {
        let resp = self.open_stream(request, true).await?;
        let (tx, rx) = oneshot::channel();
        Ok((sse_deltas(resp.bytes_stream(), self.model.clone(), Some(tx)), rx))
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
            anyhow::bail!("{} ({}): HTTP {status}: {}", self.label, self.model, error_message(&body));
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

    async fn collect(s: TextStream) -> String {
        s.map(|d| d.unwrap()).collect::<Vec<_>>().await.concat()
    }

    #[tokio::test]
    async fn server_sent_events_become_content_deltas() {
        let events = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n\
                      data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"say\"}}]}\n\ndata: {\"choi";
        let tail = "ces\":[{\"delta\":{\"content\":\"\\\": 1}\"}}]}\n\ndata: [DONE]\n\n";
        let chunks: Vec<reqwest::Result<bytes::Bytes>> = vec![Ok(events.into()), Ok(tail.into())];
        let deltas = collect(sse_deltas(futures::stream::iter(chunks), "m".into(), None)).await;
        assert_eq!(deltas, "{\"say\": 1}");
    }

    #[tokio::test]
    async fn the_last_event_reports_the_tokens() {
        let events = "data: {\"model\":\"gpt-6-sol-2026\",\"choices\":[{\"delta\":{\"content\":\"{}\"}}]}\n\n\
                      data: {\"model\":\"gpt-6-sol-2026\",\"choices\":[],\"usage\":{\"prompt_tokens\":3000,\
                      \"completion_tokens\":40,\"prompt_tokens_details\":{\"cached_tokens\":2800}}}\n\ndata: [DONE]\n\n";
        let chunks: Vec<reqwest::Result<bytes::Bytes>> = vec![Ok(events.into())];
        let (tx, rx) = oneshot::channel();
        let text = collect(sse_deltas(futures::stream::iter(chunks), "gpt-6-sol".into(), Some(tx))).await;
        assert_eq!(text, "{}");
        let usage = rx.await.unwrap();
        assert_eq!(usage, Usage { model: "gpt-6-sol-2026".into(), input: 3000, cached: 2800, output: 40 });
    }

    #[test]
    fn openai_keeps_temperature_zero_and_sends_no_reasoning_effort() {
        let body = OpenAi::new(reqwest::Client::new(), "k".into(), None, None).body(&request());
        assert_eq!(body["model"], DEFAULT_MODEL);
        assert_eq!(body["temperature"], 0.0);
        assert_eq!(body["max_tokens"], 400);
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn reasoning_models_are_told_how_hard_to_think_and_get_no_temperature() {
        let body = OpenAi::agent(reqwest::Client::new(), "k".into(), None, None, None).body(&request());
        assert_eq!(body["model"], AGENT_MODEL);
        assert_eq!(body["reasoning_effort"], "none");
        assert_eq!(body["max_completion_tokens"], 600);
        assert!(body.get("temperature").is_none());
        assert!(body.get("max_tokens").is_none());

        let low =
            OpenAi::agent(reqwest::Client::new(), "k".into(), None, Some("gpt-6-luna".into()), Some("low".into()));
        assert_eq!(low.body(&request())["max_completion_tokens"], 4000, "room for the reasoning tokens");

        // An older agent model is sent as before.
        let old = OpenAi::agent(reqwest::Client::new(), "k".into(), None, Some("gpt-4o".into()), Some("low".into()));
        let body = old.body(&request());
        assert_eq!(body["temperature"], 0.0);
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn reasoning_families() {
        for m in ["gpt-5", "gpt-5.1", "gpt-6-sol", "gpt-6-luna", "o3-mini", "o4-mini"] {
            assert!(is_reasoning_model(m), "{m}");
        }
        for m in ["gpt-4o", "gpt-4.1", "gpt-4o-mini", "gemini-3.8-flash", "llama-3"] {
            assert!(!is_reasoning_model(m), "{m}");
        }
    }
}

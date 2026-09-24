//! OpenAI-compatible chat completions with strict JSON-schema output, used only for
//! structured understanding (never to talk to the caller). `TEXT_LLM_BASE_URL` points it
//! at any compatible endpoint, as in the legacy deployment.

use async_trait::async_trait;
use serde_json::{json, Value};

use callora_core::llm::LlmRequest;
use callora_runtime::ports::LanguageModel;

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_MODEL: &str = "gpt-4o-mini";

pub struct OpenAi {
    http: reqwest::Client,
    api_key: String,
    base_url: String,
    model: String,
}

impl OpenAi {
    pub fn new(http: reqwest::Client, api_key: String, base_url: Option<String>, model: Option<String>) -> Self {
        let nonblank = |v: Option<String>| v.filter(|s| !s.trim().is_empty());
        Self {
            http,
            api_key,
            base_url: nonblank(base_url).unwrap_or_else(|| DEFAULT_BASE_URL.into()).trim_end_matches('/').into(),
            model: nonblank(model).unwrap_or_else(|| DEFAULT_MODEL.into()),
        }
    }

    pub fn body(&self, request: &LlmRequest) -> Value {
        json!({
            "model": self.model,
            "temperature": 0,
            "max_tokens": 400,
            "messages": [
                { "role": "system", "content": request.system },
                { "role": "user", "content": request.user },
            ],
            "response_format": {
                "type": "json_schema",
                "json_schema": { "name": "understanding", "strict": true, "schema": request.schema },
            },
        })
    }
}

#[async_trait]
impl LanguageModel for OpenAi {
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
            let msg = body.pointer("/error/message").and_then(Value::as_str).unwrap_or("unknown error");
            anyhow::bail!("LLM request failed with HTTP {status}: {msg}");
        }
        let content = body
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("LLM reply has no content"))?;
        Ok(serde_json::from_str(content)?)
    }

    fn name(&self) -> &'static str {
        "openai"
    }
}

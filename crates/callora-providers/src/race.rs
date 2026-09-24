//! Several understanding LLMs asked the same question at once; the first valid answer
//! wins. Understanding sits on the reply path, and on live calls one provider was often
//! slow or unavailable (Gemini answering 503 "high demand" or 429 on a free-tier key), which
//! turned every hard turn into an automatic "didn't catch that". Racing costs a second
//! request per hard turn and removes both the slowest provider's latency and its outages.

use std::sync::Arc;

use async_trait::async_trait;
use futures::future::select_ok;
use futures::FutureExt;
use serde_json::Value;

use callora_core::llm::LlmRequest;
use callora_runtime::ports::LanguageModel;

pub struct FirstAnswer {
    models: Vec<Arc<dyn LanguageModel>>,
}

impl FirstAnswer {
    pub fn new(models: Vec<Arc<dyn LanguageModel>>) -> Self {
        Self { models }
    }
}

#[async_trait]
impl LanguageModel for FirstAnswer {
    async fn extract(&self, request: &LlmRequest) -> anyhow::Result<Value> {
        if self.models.is_empty() {
            anyhow::bail!("no language model configured");
        }
        let attempts = self.models.iter().map(|m| {
            let name = m.name();
            m.extract(request).map(move |r| r.map_err(|e| e.context(name))).boxed()
        });
        match select_ok(attempts).await {
            Ok((reply, _slower)) => Ok(reply),
            // select_ok returns the last error; the others were logged by nobody, so say
            // that every provider failed.
            Err(e) => Err(e.context("every language model failed")),
        }
    }

    fn name(&self) -> &'static str {
        "first-answer"
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::*;

    struct Fake {
        delay_ms: u64,
        reply: Result<Value, &'static str>,
        name: &'static str,
    }

    #[async_trait]
    impl LanguageModel for Fake {
        async fn extract(&self, _request: &LlmRequest) -> anyhow::Result<Value> {
            tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
            self.reply.clone().map_err(anyhow::Error::msg)
        }
        fn name(&self) -> &'static str {
            self.name
        }
    }

    fn request() -> LlmRequest {
        LlmRequest { system: String::new(), user: String::new(), schema: json!({}) }
    }

    fn race(models: Vec<Fake>) -> FirstAnswer {
        FirstAnswer::new(models.into_iter().map(|m| Arc::new(m) as Arc<dyn LanguageModel>).collect())
    }

    #[tokio::test(start_paused = true)]
    async fn the_fastest_valid_answer_wins_and_a_failure_does_not_count() {
        let r = race(vec![
            Fake { delay_ms: 10, reply: Err("503 high demand"), name: "gemini" },
            Fake { delay_ms: 900, reply: Ok(json!({ "from": "openai" })), name: "openai" },
            Fake { delay_ms: 2000, reply: Ok(json!({ "from": "slow" })), name: "slow" },
        ]);
        assert_eq!(r.extract(&request()).await.unwrap(), json!({ "from": "openai" }));
    }

    #[tokio::test(start_paused = true)]
    async fn all_failing_is_an_error() {
        let r = race(vec![
            Fake { delay_ms: 5, reply: Err("429"), name: "gemini" },
            Fake { delay_ms: 5, reply: Err("timeout"), name: "openai" },
        ]);
        let e = r.extract(&request()).await.unwrap_err();
        assert!(format!("{e:#}").contains("every language model failed"), "{e:#}");
    }
}

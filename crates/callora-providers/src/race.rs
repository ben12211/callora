//! Several understanding LLMs asked the same question at once; the first valid answer
//! wins. Understanding sits on the reply path, and on live calls one provider was often
//! slow or unavailable (Gemini answering 503 "high demand" or 429 on a free-tier key), which
//! turned every hard turn into an automatic "didn't catch that". Racing costs a second
//! request per hard turn and removes both the slowest provider's latency and its outages.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::future::{select_ok, BoxFuture};
use futures::{FutureExt, StreamExt};
use serde_json::Value;

use callora_core::llm::LlmRequest;
use callora_runtime::ports::{LanguageModel, TextStream};

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

/// The agent's model with a hedge: when the primary has produced nothing after `after`
/// (or fails), the same request goes to the backup, and whichever speaks first is used.
/// Measured on the agent prompt, gpt-4.1's first words take ~620 ms and occasionally over a
/// second; the hedge caps that tail at about `after` plus the backup's own time.
pub struct Hedged {
    primary: Arc<dyn LanguageModel>,
    backup: Arc<dyn LanguageModel>,
    after: Duration,
}

impl Hedged {
    pub fn new(primary: Arc<dyn LanguageModel>, backup: Arc<dyn LanguageModel>, after: Duration) -> Self {
        Self { primary, backup, after }
    }
}

/// A model's stream once its first delta is in.
type Started = (String, TextStream);

fn start(model: Arc<dyn LanguageModel>, request: LlmRequest) -> BoxFuture<'static, anyhow::Result<Started>> {
    async move {
        let name = model.name();
        let mut stream = model.stream(&request).await.map_err(|e| e.context(name))?;
        match stream.next().await {
            Some(Ok(first)) => Ok((first, stream)),
            Some(Err(e)) => Err(e.context(name)),
            None => Err(anyhow::anyhow!("{name}: empty reply")),
        }
    }
    .boxed()
}

fn resume((first, rest): Started) -> TextStream {
    futures::stream::once(async move { Ok(first) }).chain(rest).boxed()
}

#[async_trait]
impl LanguageModel for Hedged {
    async fn extract(&self, request: &LlmRequest) -> anyhow::Result<Value> {
        match self.primary.extract(request).await {
            Ok(v) => Ok(v),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "primary model failed; asking the backup");
                self.backup.extract(request).await
            }
        }
    }

    async fn stream(&self, request: &LlmRequest) -> anyhow::Result<TextStream> {
        let mut primary = start(self.primary.clone(), request.clone());
        tokio::select! {
            r = &mut primary => match r {
                Ok(started) => return Ok(resume(started)),
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "primary model failed; asking the backup");
                    return start(self.backup.clone(), request.clone()).await.map(resume);
                }
            },
            () = tokio::time::sleep(self.after) => {}
        }
        tracing::info!(after_ms = self.after.as_millis() as u64, "primary model is slow; hedging with the backup");
        let backup = start(self.backup.clone(), request.clone());
        select_ok([primary, backup]).await.map(|(started, _)| resume(started))
    }

    async fn warm(&self) {
        futures::join!(self.primary.warm(), self.backup.warm());
    }

    fn name(&self) -> &'static str {
        "hedged"
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

    async fn text(s: TextStream) -> String {
        s.map(|d| d.unwrap()).collect::<Vec<_>>().await.concat()
    }

    fn hedge(primary: Fake, backup: Fake) -> Hedged {
        Hedged::new(Arc::new(primary), Arc::new(backup), Duration::from_millis(900))
    }

    #[tokio::test(start_paused = true)]
    async fn a_fast_primary_is_used_without_the_backup() {
        let h = hedge(
            Fake { delay_ms: 600, reply: Ok(json!("primary")), name: "gpt-4.1" },
            Fake { delay_ms: 10, reply: Ok(json!("backup")), name: "gpt-4o" },
        );
        assert_eq!(text(h.stream(&request()).await.unwrap()).await, "\"primary\"");
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_primary_is_overtaken_by_the_backup() {
        let h = hedge(
            Fake { delay_ms: 3000, reply: Ok(json!("primary")), name: "gpt-4.1" },
            Fake { delay_ms: 500, reply: Ok(json!("backup")), name: "gpt-4o" },
        );
        let started = tokio::time::Instant::now();
        assert_eq!(text(h.stream(&request()).await.unwrap()).await, "\"backup\"");
        assert!(started.elapsed() < Duration::from_millis(1500), "{:?}", started.elapsed());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_primary_goes_straight_to_the_backup() {
        let h = hedge(
            Fake { delay_ms: 50, reply: Err("500"), name: "gpt-4.1" },
            Fake { delay_ms: 500, reply: Ok(json!("backup")), name: "gpt-4o" },
        );
        assert_eq!(text(h.stream(&request()).await.unwrap()).await, "\"backup\"");
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

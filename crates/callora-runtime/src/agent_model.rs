//! The agent's LLM, chosen on the settings page: which model answers (and which one it is
//! hedged by) changes without a deployment. The calls hold one [`SwitchableModel`]; a new choice
//! is tried with a small request first and swapped in only when it answers, so a typo in a model
//! name never reaches a caller. Turns already running finish on the model they started with.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use callora_core::llm::LlmRequest;

use crate::ports::{LanguageModel, TextStream, UsageReceiver};

/// "No hedge": the agent's model answers alone.
pub const NO_BACKUP: &str = "none";

/// How much a reasoning model thinks before its first word.
pub const EFFORTS: [&str; 4] = ["none", "low", "medium", "high"];

/// How long a model has to answer the check before it is refused.
const PROBE_TIMEOUT: Duration = Duration::from_secs(25);

/// What the owner chose: the model that runs the conversation, the one that backs it up after
/// `AGENT_HEDGE_MS` of silence, and how much a reasoning model thinks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentModelSettings {
    pub primary: String,
    /// A model name, or [`NO_BACKUP`].
    #[serde(default = "no_backup")]
    pub backup: String,
    /// One of [`EFFORTS`]; absent leaves each provider's own default.
    #[serde(default)]
    pub effort: Option<String>,
}

fn no_backup() -> String {
    NO_BACKUP.into()
}

/// Whether the model is Gemini's (a `gemini*` name); any other goes to OpenAI.
pub fn is_gemini(model: &str) -> bool {
    model.starts_with("gemini")
}

fn valid_name(model: &str) -> bool {
    let mut chars = model.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && model.len() <= 64
        && model.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
}

impl AgentModelSettings {
    /// Names trimmed, an empty backup meaning none, an empty effort meaning the default.
    pub fn normalized(mut self) -> Self {
        self.primary = self.primary.trim().to_string();
        self.backup = self.backup.trim().to_string();
        if self.backup.is_empty() {
            self.backup = no_backup();
        }
        self.effort = self.effort.map(|e| e.trim().to_string()).filter(|e| !e.is_empty());
        self
    }

    /// The models these settings use: the agent's, then its backup, if there is one.
    pub fn models(&self) -> Vec<&str> {
        let mut m = vec![self.primary.as_str()];
        if self.backup != NO_BACKUP {
            m.push(self.backup.as_str());
        }
        m
    }

    /// Problems that keep these settings from being used.
    pub fn problems(&self) -> Vec<String> {
        let mut p = Vec::new();
        if !valid_name(&self.primary) {
            p.push("שם המודל הראשי לא תקין (אותיות לטיניות, ספרות, נקודה, מקף, עד 64 תווים)".into());
        }
        if self.backup != NO_BACKUP && !valid_name(&self.backup) {
            p.push("שם מודל הגיבוי לא תקין".into());
        }
        if self.effort.as_deref().is_some_and(|e| !EFFORTS.contains(&e)) {
            p.push("רמת החשיבה: ללא, נמוכה, בינונית או גבוהה".into());
        }
        p
    }
}

/// Which providers the server has a key for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Providers {
    pub gemini: bool,
    pub openai: bool,
}

impl Providers {
    pub fn has(&self, model: &str) -> bool {
        if is_gemini(model) {
            self.gemini
        } else {
            self.openai
        }
    }
}

/// The model the calls ask, replaceable while they run.
pub struct SwitchableModel {
    inner: RwLock<Arc<dyn LanguageModel>>,
}

impl SwitchableModel {
    pub fn new(model: Arc<dyn LanguageModel>) -> Self {
        Self { inner: RwLock::new(model) }
    }

    pub fn set(&self, model: Arc<dyn LanguageModel>) {
        *self.inner.write() = model;
    }

    fn current(&self) -> Arc<dyn LanguageModel> {
        self.inner.read().clone()
    }
}

#[async_trait]
impl LanguageModel for SwitchableModel {
    async fn extract(&self, request: &LlmRequest) -> anyhow::Result<Value> {
        self.current().extract(request).await
    }

    async fn stream(&self, request: &LlmRequest) -> anyhow::Result<TextStream> {
        self.current().stream(request).await
    }

    async fn stream_metered(&self, request: &LlmRequest) -> anyhow::Result<(TextStream, UsageReceiver)> {
        self.current().stream_metered(request).await
    }

    async fn warm(&self) {
        self.current().warm().await;
    }

    fn name(&self) -> &'static str {
        "agent"
    }
}

/// One model by name and thinking effort, `None` without its provider's key.
pub type NamedModel = Arc<dyn Fn(&str, Option<String>) -> Option<Arc<dyn LanguageModel>> + Send + Sync>;
/// The whole agent for some settings: the model, hedged by its backup, with its fallback.
pub type AssembledModel = Arc<dyn Fn(&AgentModelSettings) -> Option<Arc<dyn LanguageModel>> + Send + Sync>;

/// What the settings page drives: the live model and how to build another.
pub struct AgentControl {
    handle: Arc<SwitchableModel>,
    named: NamedModel,
    assemble: AssembledModel,
    /// What the environment says (`AGENT_MODEL` and the others): where "back to default" goes.
    defaults: AgentModelSettings,
    active: RwLock<AgentModelSettings>,
    providers: Providers,
    /// Models offered by name on the page; any other name can be typed.
    catalog: Vec<String>,
}

impl AgentControl {
    pub fn new(
        handle: Arc<SwitchableModel>,
        active: AgentModelSettings,
        defaults: AgentModelSettings,
        named: NamedModel,
        assemble: AssembledModel,
        providers: Providers,
        catalog: Vec<String>,
    ) -> Self {
        Self { handle, named, assemble, defaults, active: RwLock::new(active), providers, catalog }
    }

    /// What the calls ask.
    pub fn model(&self) -> Arc<dyn LanguageModel> {
        self.handle.clone()
    }

    pub fn defaults(&self) -> &AgentModelSettings {
        &self.defaults
    }

    pub fn active(&self) -> AgentModelSettings {
        self.active.read().clone()
    }

    /// For the settings page: what is in use, what the default is, and what can be chosen.
    pub fn view(&self) -> Value {
        let active = self.active();
        let mut wanted: Vec<&str> = self.catalog.iter().map(String::as_str).collect();
        wanted.extend(self.defaults.models());
        wanted.extend(active.models());
        let mut models: Vec<Value> = Vec::new();
        let mut seen: Vec<&str> = Vec::new();
        for m in wanted {
            if !seen.contains(&m) {
                seen.push(m);
                models.push(json!({ "id": m, "provider": if is_gemini(m) { "gemini" } else { "openai" } }));
            }
        }
        json!({
            "active": active,
            "defaults": self.defaults,
            "custom": active != self.defaults,
            "providers": self.providers,
            "models": models,
        })
    }

    /// Starts using `wanted` for the calls' next turns, once each of its models has answered a
    /// small request. The reasons it was refused, in words, otherwise.
    pub async fn switch(&self, wanted: AgentModelSettings) -> Result<(), Vec<String>> {
        let wanted = wanted.normalized();
        let mut problems = wanted.problems();
        for m in wanted.models() {
            if !self.providers.has(m) {
                let who = if is_gemini(m) { "Gemini" } else { "OpenAI" };
                problems.push(format!("אין בשרת מפתח של {who}, ולכן אי אפשר להשתמש ב-{m}"));
            }
        }
        if !problems.is_empty() {
            return Err(problems);
        }
        let same = self.active() == wanted;
        if same {
            return Ok(());
        }
        let mut checks = Vec::new();
        for m in wanted.models() {
            match (self.named)(m, wanted.effort.clone()) {
                Some(model) => checks.push(probe(model, m.to_string())),
                None => problems.push(format!("המודל {m} לא נבנה")),
            }
        }
        for failed in futures::future::join_all(checks).await.into_iter().filter_map(Result::err) {
            problems.push(failed);
        }
        if !problems.is_empty() {
            return Err(problems);
        }
        let Some(assembled) = (self.assemble)(&wanted) else {
            return Err(vec!["לא ניתן להרכיב את הסוכן מהמודלים שנבחרו".into()]);
        };
        tracing::info!(
            primary = %wanted.primary,
            backup = %wanted.backup,
            effort = ?wanted.effort,
            "the agent's model was changed"
        );
        self.handle.set(assembled.clone());
        *self.active.write() = wanted;
        // The connection is opened now, not on the first caller's turn.
        tokio::spawn(async move { assembled.warm().await });
        Ok(())
    }
}

/// A model asked a question with a one-word answer: it exists, the key works, it answers in time.
async fn probe(model: Arc<dyn LanguageModel>, name: String) -> Result<(), String> {
    let request = LlmRequest {
        system: "Answer with the JSON object only.".into(),
        user: "Reply with {\"ok\": true}.".into(),
        schema: json!({
            "type": "object",
            "properties": { "ok": { "type": "boolean" } },
            "required": ["ok"],
            "additionalProperties": false,
        }),
    };
    match tokio::time::timeout(PROBE_TIMEOUT, model.extract(&request)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => {
            let why: String = format!("{e:#}").chars().take(200).collect();
            Err(format!("{name} לא ענה: {why}"))
        }
        Err(_) => Err(format!("{name} לא ענה תוך {} שניות", PROBE_TIMEOUT.as_secs())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(&'static str);

    #[async_trait]
    impl LanguageModel for Fixed {
        async fn extract(&self, _request: &LlmRequest) -> anyhow::Result<Value> {
            if self.0 == "broken" {
                anyhow::bail!("HTTP 404: model not found");
            }
            Ok(json!({ "ok": true, "by": self.0 }))
        }

        fn name(&self) -> &'static str {
            self.0
        }
    }

    fn request() -> LlmRequest {
        LlmRequest { system: "s".into(), user: "u".into(), schema: json!({ "type": "object" }) }
    }

    fn settings(primary: &str, backup: &str) -> AgentModelSettings {
        AgentModelSettings { primary: primary.into(), backup: backup.into(), effort: None }
    }

    fn control(openai: bool) -> AgentControl {
        let handle = Arc::new(SwitchableModel::new(Arc::new(Fixed("first"))));
        // "broken" is a model name the provider refuses.
        let named: NamedModel = Arc::new(|m, _| {
            let name: &'static str = if m == "broken" { "broken" } else { "second" };
            Some(Arc::new(Fixed(name)) as Arc<dyn LanguageModel>)
        });
        let assemble: AssembledModel = Arc::new(|_| Some(Arc::new(Fixed("second")) as Arc<dyn LanguageModel>));
        AgentControl::new(
            handle,
            settings("gemini-3.8-flash", "gpt-6-luna"),
            settings("gemini-3.8-flash", "gpt-6-luna"),
            named,
            assemble,
            Providers { gemini: true, openai },
            vec!["gemini-3.8-flash".into(), "gpt-6-sol".into()],
        )
    }

    #[test]
    fn names_and_efforts_are_checked() {
        assert!(settings("gpt-6-sol", NO_BACKUP).problems().is_empty());
        assert!(settings("gemini-3.8-flash", "gpt-6-luna").problems().is_empty());
        assert_eq!(settings("", NO_BACKUP).problems().len(), 1);
        assert_eq!(settings("gpt 6", "a/b").problems().len(), 2, "spaces and slashes are not in model names");
        let effort = |e: &str| AgentModelSettings { effort: Some(e.into()), ..settings("m", NO_BACKUP) };
        assert!(effort("low").problems().is_empty());
        assert_eq!(effort("max").problems().len(), 1);
        let loose = AgentModelSettings { effort: Some(" ".into()), ..settings(" gpt-6-sol ", " ") }.normalized();
        assert_eq!((loose.primary.as_str(), loose.backup.as_str(), loose.effort), ("gpt-6-sol", NO_BACKUP, None));
        assert_eq!(settings("a", NO_BACKUP).models(), vec!["a"]);
        assert_eq!(settings("a", "b").models(), vec!["a", "b"]);
    }

    #[tokio::test]
    async fn a_model_is_swapped_in_only_after_it_answers() {
        let c = control(false);
        assert_eq!(c.model().extract(&request()).await.unwrap()["by"], "first");
        c.switch(settings("gemini-3.8-flash", NO_BACKUP)).await.unwrap();
        assert_eq!(c.model().extract(&request()).await.unwrap()["by"], "second");
        assert_eq!(c.active().backup, NO_BACKUP);
    }

    #[tokio::test]
    async fn a_model_that_fails_or_has_no_key_is_refused_and_the_old_one_stays() {
        let c = control(true);
        let refused = c.switch(settings("broken", NO_BACKUP)).await.unwrap_err();
        assert!(refused[0].contains("לא ענה") && refused[0].contains("broken"), "{refused:?}");
        assert_eq!(c.model().extract(&request()).await.unwrap()["by"], "first", "still the first model");
        assert_eq!(c.active().primary, "gemini-3.8-flash");

        let c = control(false);
        let no_key = c.switch(settings("gpt-6-sol", NO_BACKUP)).await.unwrap_err();
        assert!(no_key[0].contains("OpenAI"), "{no_key:?}");
        assert_eq!(c.active().primary, "gemini-3.8-flash");
    }

    #[test]
    fn the_page_is_told_what_is_in_use_and_what_can_be_chosen() {
        let v = control(false).view();
        assert_eq!(v["custom"], false);
        assert_eq!(v["providers"], json!({ "gemini": true, "openai": false }));
        let ids: Vec<&str> = v["models"].as_array().unwrap().iter().filter_map(|m| m["id"].as_str()).collect();
        assert_eq!(ids, vec!["gemini-3.8-flash", "gpt-6-sol", "gpt-6-luna"], "the catalog, then what is in use");
    }
}

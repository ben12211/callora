//! `callora eval`: the agent, run against recorded conversations with the real model.
//!
//! The unit tests script the model's replies, so they prove the engine enforces its rules
//! but say nothing about how the model behaves. Every conversation that went wrong on a
//! live call becomes a case here (`evaluation/agent/*.json`): the caller's words, turn by
//! turn, and what must (and must not) happen. A case runs through the same engine, fast
//! lane, place checks and mock actions as a call, with the model the call would use, and
//! each case runs several times because the model is not deterministic. The report gives
//! the pass rate, what failed, how fast the first words came and what the tokens cost, so
//! a prompt or model change is measured on all known cases before it reaches a caller.
//!
//! A case file is a JSON array of cases:
//!
//! ```json
//! [{ "id": "betar_is_betar_illit",
//!    "note": "live call 2026-09-26 booked רימון 16, מיתר",
//!    "turns": [
//!      { "caller": "צריך מונית", "scripted": { "action": "none", "task": "book_ride", "say": "מאיזו עיר לאסוף?" } },
//!      { "caller": "מביתר", "expect": { "cities": { "pickup": "ביתר עילית" }, "says_any": ["רחוב"] } } ] }]
//! ```
//!
//! A `scripted` turn is a fixed decision (the scene before the turn under test) and never
//! calls the model. Alternatives inside one expected string are separated by `|`.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use callora_core::agent::{self, AgentAction, AgentTurn, SayStream};
use callora_core::business::{Business, BusinessRegistry};
use callora_core::customer::Customer;
use callora_core::engine::{Directive, Engine};
use callora_core::gazetteer::Gazetteer;
use callora_core::state::Step;
use callora_core::understanding::fast_path;
use callora_runtime::ports::{ActionRunner, CallInfo, LanguageModel, Usage};
use callora_runtime::pricing::{cost, Prices};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub id: String,
    /// Where the case came from (a live call, a date, what went wrong).
    #[serde(default)]
    pub note: String,
    #[serde(default = "default_business")]
    pub business: String,
    /// A known caller, as the customer lookup would return them.
    #[serde(default)]
    pub customer: Option<Customer>,
    pub turns: Vec<CaseTurn>,
}

fn default_business() -> String {
    "taxi".into()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaseTurn {
    /// What the recognizer heard.
    pub caller: String,
    /// What a second, hinted transcription heard, when the call had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub second_hearing: Option<String>,
    /// A fixed decision instead of the model's (setting the scene).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scripted: Option<Value>,
    #[serde(default, skip_serializing_if = "Expect::is_empty")]
    pub expect: Expect,
}

/// What must be true after a turn. Every field is optional.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    /// The model's action is one of these (`none`, `read_back`, `submit`, `transfer`, `end_call`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub action: Vec<String>,
    /// The task the model names ("" for none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// Details the model passes this turn: slot → text its value contains.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, String>,
    /// Slots the model must not pass this turn (it would be inventing them).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub no_fields: Vec<String>,
    /// What the caller hears this turn contains one of these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub says_any: Vec<String>,
    /// ... and none of these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub says_none: Vec<String>,
    /// The task's details after the turn: slot → text the stored value contains ("" = set).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub slots: BTreeMap<String, String>,
    /// Slots still without a value after the turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<String>,
    /// A place slot whose city is noted while its street is still asked: slot → city.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub cities: BTreeMap<String, String>,
    /// `none`, `collecting`, `confirming_slot`, `awaiting_confirmation` or `executing`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    /// A business action ran this turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submitted: Option<bool>,
    /// The call was hung up this turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<bool>,
    /// The call was handed to a human this turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<bool>,
}

impl Expect {
    fn is_empty(&self) -> bool {
        *self == Expect::default()
    }
}

/// Loads every case file under `dir` (or one file).
pub fn load_cases(path: &Path) -> anyhow::Result<Vec<Case>> {
    let mut files = Vec::new();
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            let p = entry?.path();
            if p.extension().is_some_and(|e| e == "json") {
                files.push(p);
            }
        }
        files.sort();
    } else {
        files.push(path.to_path_buf());
    }
    let mut cases = Vec::new();
    for f in files {
        let text = std::fs::read_to_string(&f)?;
        let mut these: Vec<Case> = serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", f.display()))?;
        cases.append(&mut these);
    }
    let mut seen = std::collections::HashSet::new();
    if let Some(dup) = cases.iter().find(|c| !seen.insert(c.id.clone())) {
        anyhow::bail!("case id `{}` appears twice", dup.id);
    }
    Ok(cases)
}

/// Problems in a case that no model could fix: unknown business, slots, tasks or actions.
pub fn check_case(case: &Case, registry: &BusinessRegistry) -> Vec<String> {
    let mut problems = Vec::new();
    let Some(b) = registry.by_id(&case.business) else {
        return vec![format!("{}: unknown business `{}`", case.id, case.business)];
    };
    let slot_known = |s: &str| b.config.slots.contains_key(s);
    let actions: Vec<&str> = ["none", "read_back", "submit", "transfer", "end_call"].to_vec();
    let steps = ["none", "collecting", "confirming_slot", "awaiting_confirmation", "executing"];
    if case.turns.is_empty() {
        problems.push(format!("{}: no turns", case.id));
    }
    if !case.turns.iter().any(|t| t.scripted.is_none()) {
        problems.push(format!("{}: every turn is scripted, nothing is tested", case.id));
    }
    for (i, t) in case.turns.iter().enumerate() {
        let at = format!("{} turn {}", case.id, i + 1);
        if let Some(s) = &t.scripted {
            for key in s.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() {
                if !["action", "task", "fields", "say", "phrase"].contains(&key.as_str()) {
                    problems.push(format!("{at}: scripted has an unknown key `{key}`"));
                }
            }
            let turn = agent::parse(&b, s);
            if let Some(task) = s.get("task").and_then(Value::as_str) {
                if turn.task.is_none() {
                    problems.push(format!("{at}: scripted task `{task}` is not a task of the business"));
                }
            }
            if s.get("fields").and_then(Value::as_array).map_or(0, Vec::len) != turn.fields.len() {
                problems.push(format!("{at}: a scripted field names an unknown slot"));
            }
            if t.expect != Expect::default() {
                problems.push(format!("{at}: a scripted turn has expectations (they would test the script)"));
            }
        }
        let e = &t.expect;
        for a in &e.action {
            if !actions.contains(&a.as_str()) {
                problems.push(format!("{at}: unknown action `{a}`"));
            }
        }
        if let Some(task) = e.task.as_deref().filter(|t| !t.is_empty()) {
            if b.intent(task).is_none() {
                problems.push(format!("{at}: unknown task `{task}`"));
            }
        }
        for s in e.fields.keys().chain(&e.no_fields).chain(e.slots.keys()).chain(&e.missing).chain(e.cities.keys()) {
            if !slot_known(s) {
                problems.push(format!("{at}: unknown slot `{s}`"));
            }
        }
        if let Some(step) = &e.step {
            if !steps.contains(&step.as_str()) {
                problems.push(format!("{at}: unknown step `{step}`"));
            }
        }
    }
    problems
}

/// How the turn was decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    /// The model decided.
    Agent,
    /// A fixed decision from the case.
    Scripted,
    /// A certain meaning ("מה?", a plain yes to the read-back): the engine alone.
    FastLane,
    /// Filler words only, ignored as line noise.
    Noise,
}

#[derive(Debug, Clone, Serialize)]
pub struct TurnRun {
    pub caller: String,
    pub route: Route,
    /// The model's raw reply.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply: Option<Value>,
    /// Everything the caller heard this turn.
    pub heard: String,
    /// From the request to the first words that could play.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_words_ms: Option<u64>,
    /// From the request to the complete decision.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    pub failures: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CaseRun {
    pub case: String,
    pub passed: bool,
    pub turns: Vec<TurnRun>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub struct Runner {
    pub registry: Arc<BusinessRegistry>,
    pub gazetteer: Option<Arc<Gazetteer>>,
    pub actions: Arc<dyn ActionRunner>,
    pub pacer: Option<Arc<Pacer>>,
}

/// What the directives of one turn did.
#[derive(Default)]
struct Effects {
    heard: Vec<String>,
    submitted: bool,
    ended: bool,
    handoff: bool,
}

impl Runner {
    pub async fn run_case(&self, case: &Case, model: &dyn LanguageModel, seed: u64) -> CaseRun {
        let mut run = CaseRun { case: case.id.clone(), passed: false, turns: Vec::new(), error: None };
        let Some(b) = self.registry.by_id(&case.business) else {
            run.error = Some(format!("unknown business `{}`", case.business));
            return run;
        };
        let info = CallInfo {
            call_id: uuid_from(seed),
            call_sid: format!("EVAL-{}", case.id),
            business_id: b.config.id.clone(),
            from: Some("+972500000000".into()),
            to: String::new(),
        };
        let mut engine = Engine::new(b.clone(), seed);
        engine.set_gazetteer(self.gazetteer.clone());
        engine.set_caller_phone(info.from.clone());
        engine.set_customer(case.customer.clone());
        let greeting = engine.start();
        self.drive(&b, &mut engine, &info, greeting).await;

        for t in &case.turns {
            let mut turn = TurnRun {
                caller: t.caller.clone(),
                route: Route::Agent,
                reply: None,
                heard: String::new(),
                first_words_ms: None,
                decision_ms: None,
                usage: None,
                failures: Vec::new(),
            };
            let mut decision: Option<AgentTurn> = None;
            let directives = if let Some(s) = &t.scripted {
                turn.route = Route::Scripted;
                let d = agent::parse(&b, s);
                engine.on_agent_turn(&t.caller, d, "")
            } else {
                let (u, needs_llm) = fast_path(&b, &engine.context(), &t.caller);
                if u.noise {
                    turn.route = Route::Noise;
                    Vec::new()
                } else if engine.fast_lane(&u, needs_llm) {
                    turn.route = Route::FastLane;
                    engine.on_utterance(u)
                } else {
                    engine.state.second_hearing = t.second_hearing.clone();
                    let request = agent::build_request(&b, &engine.state, &t.caller);
                    if let Some(p) = &self.pacer {
                        p.wait_turn().await;
                    }
                    match ask(model, &request).await {
                        Ok(asked) => {
                            turn.first_words_ms = asked.first_words_ms;
                            turn.decision_ms = Some(asked.decision_ms);
                            turn.usage = asked.usage;
                            let d = agent::parse(&b, &asked.reply);
                            turn.reply = Some(asked.reply);
                            decision = Some(d.clone());
                            engine.on_agent_turn(&t.caller, d, "")
                        }
                        Err(e) => {
                            turn.failures.push(format!("the model failed: {e:#}"));
                            run.error = Some(format!("{e:#}"));
                            run.turns.push(turn);
                            return run;
                        }
                    }
                }
            };
            let effects = self.drive(&b, &mut engine, &info, directives).await;
            turn.heard = effects.heard.join(" ");
            turn.failures = check(&t.expect, decision.as_ref(), turn.route, &effects, &engine, &turn.heard);
            let ended = effects.ended || effects.handoff;
            run.turns.push(turn);
            if ended {
                break;
            }
        }
        run.passed = run.error.is_none() && run.turns.iter().all(|t| t.failures.is_empty());
        if run.turns.len() < case.turns.len() && run.passed {
            run.passed = false;
            let left = case.turns.len() - run.turns.len();
            if let Some(last) = run.turns.last_mut() {
                last.failures.push(format!("the call ended with {left} turns of the case left"));
            }
        }
        run
    }

    /// Executes directives the way a call does: speech is collected, actions run (mock
    /// backends unless their URLs are set) and their results go back to the engine.
    async fn drive(&self, b: &Arc<Business>, engine: &mut Engine, info: &CallInfo, first: Vec<Directive>) -> Effects {
        let mut fx = Effects::default();
        let mut pending = first;
        while !pending.is_empty() {
            let mut next = Vec::new();
            for d in std::mem::take(&mut pending) {
                match d {
                    Directive::Speak { plan, .. } => fx.heard.push(plan.text()),
                    Directive::RunAction { run_id, action, input } => {
                        fx.submitted = true;
                        let result = self.actions.run(b, &action, input, info).await;
                        next.extend(engine.on_action_result(run_id, result));
                    }
                    Directive::Handoff { .. } => fx.handoff = true,
                    Directive::Hangup => fx.ended = true,
                }
            }
            pending = next;
        }
        fx
    }
}

fn uuid_from(seed: u64) -> uuid::Uuid {
    uuid::Uuid::from_u64_pair(seed, seed.rotate_left(17))
}

struct Asked {
    reply: Value,
    first_words_ms: Option<u64>,
    decision_ms: u64,
    usage: Option<Usage>,
}

/// One request, streamed as a call streams it, to time the first words.
async fn ask(model: &dyn LanguageModel, request: &callora_core::llm::LlmRequest) -> anyhow::Result<Asked> {
    let started = Instant::now();
    let (mut stream, usage) = model.stream_metered(request).await?;
    let mut say = SayStream::default();
    let mut raw = String::new();
    let mut first_words = None;
    while let Some(delta) = stream.next().await {
        let delta = delta?;
        raw.push_str(&delta);
        let sentences = say.push(&delta);
        let spoken_now = !sentences.is_empty() || say.phrase().is_some();
        let held = matches!(say.action(), Some(AgentAction::ReadBack | AgentAction::Submit | AgentAction::EndCall));
        if first_words.is_none() && spoken_now && !held {
            first_words = Some(started.elapsed().as_millis() as u64);
        }
    }
    let decision_ms = started.elapsed().as_millis() as u64;
    let reply: Value = serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("the reply is not JSON ({e}): {raw}"))?;
    let usage = tokio::time::timeout(std::time::Duration::from_millis(200), usage).await.ok().and_then(Result::ok);
    Ok(Asked { reply, first_words_ms: first_words.or(Some(decision_ms)), decision_ms, usage })
}

fn alternatives(expected: &str) -> impl Iterator<Item = &str> {
    expected.split('|').map(str::trim)
}

fn contains_any(haystack: &str, expected: &str) -> bool {
    let h = callora_core::text::normalize(haystack);
    alternatives(expected).any(|a| a.is_empty() || h.contains(&callora_core::text::normalize(a)))
}

fn step_name(engine: &Engine) -> &'static str {
    match engine.state.run.as_ref().map(|r| &r.step) {
        None => "none",
        Some(Step::Collecting { .. }) => "collecting",
        Some(Step::ConfirmingSlot { .. }) => "confirming_slot",
        Some(Step::AwaitingConfirmation) => "awaiting_confirmation",
        Some(Step::Executing { .. }) => "executing",
    }
}

/// The value of a slot: in the task under way, or else in the last one finished.
fn slot_value(engine: &Engine, slot: &str) -> Option<String> {
    if let Some(run) = &engine.state.run {
        return run.slots.get(slot).map(|s| s.value.spoken());
    }
    engine.state.completed.last().and_then(|c| c.slots.get(slot)).map(|v| v.spoken())
}

fn check(
    e: &Expect,
    decision: Option<&AgentTurn>,
    route: Route,
    fx: &Effects,
    engine: &Engine,
    heard: &str,
) -> Vec<String> {
    let mut f = Vec::new();
    let action_name = |a: AgentAction| match a {
        AgentAction::None => "none",
        AgentAction::ReadBack => "read_back",
        AgentAction::Submit => "submit",
        AgentAction::Transfer => "transfer",
        AgentAction::EndCall => "end_call",
    };
    // On every turn the model decided: one question at a time.
    if route == Route::Agent && heard.matches('?').count() >= 2 {
        f.push(format!("asked two questions: \"{heard}\""));
    }
    let needs_model = !e.action.is_empty() || e.task.is_some() || !e.fields.is_empty();
    match decision {
        None if needs_model => {
            f.push(format!("the model was not asked ({route:?}), so its decision cannot be checked"))
        }
        None => {}
        Some(d) => {
            if !e.action.is_empty() && !e.action.iter().any(|a| a == action_name(d.action)) {
                f.push(format!("action {} (expected {})", action_name(d.action), e.action.join(" or ")));
            }
            if let Some(task) = &e.task {
                let got = d.task.clone().unwrap_or_default();
                if got != *task {
                    f.push(format!("task `{got}` (expected `{task}`)"));
                }
            }
            for (slot, want) in &e.fields {
                match d.fields.iter().find(|(s, _)| s == slot) {
                    Some((_, v)) if contains_any(v, want) => {}
                    Some((_, v)) => f.push(format!("field {slot} = \"{v}\" (expected \"{want}\")")),
                    None => f.push(format!("field {slot} not passed (expected \"{want}\")")),
                }
            }
            for slot in &e.no_fields {
                if let Some((_, v)) = d.fields.iter().find(|(s, _)| s == slot) {
                    f.push(format!("field {slot} = \"{v}\" was passed but the caller never gave it"));
                }
            }
        }
    }
    if !e.says_any.is_empty() && !e.says_any.iter().any(|s| contains_any(heard, s)) {
        f.push(format!("heard \"{heard}\" (expected one of: {})", e.says_any.join(", ")));
    }
    for s in &e.says_none {
        if alternatives(s).any(|a| !a.is_empty() && contains_any(heard, a)) {
            f.push(format!("heard \"{heard}\" (must not say \"{s}\")"));
        }
    }
    for (slot, want) in &e.slots {
        match slot_value(engine, slot) {
            Some(v) if contains_any(&v, want) => {}
            Some(v) => f.push(format!("{slot} is \"{v}\" (expected \"{want}\")")),
            None => f.push(format!("{slot} has no value (expected \"{want}\")")),
        }
    }
    for slot in &e.missing {
        if let Some(v) = slot_value(engine, slot) {
            f.push(format!("{slot} is \"{v}\" (expected no value yet)"));
        }
    }
    for (slot, want) in &e.cities {
        match engine.state.place_cities.get(slot) {
            Some(city) if contains_any(city, want) => {}
            Some(city) => f.push(format!("{slot} city noted as \"{city}\" (expected \"{want}\")")),
            None => f.push(format!("no city noted for {slot} (expected \"{want}\")")),
        }
    }
    if let Some(step) = &e.step {
        if step_name(engine) != step {
            f.push(format!("step {} (expected {step})", step_name(engine)));
        }
    }
    let flag = |f: &mut Vec<String>, what: &str, want: Option<bool>, got: bool| {
        if let Some(w) = want.filter(|w| *w != got) {
            f.push(format!("{what}: {got} (expected {w})"));
        }
    };
    flag(&mut f, "submitted", e.submitted, fx.submitted);
    flag(&mut f, "ended", e.ended, fx.ended);
    flag(&mut f, "handed off", e.handoff, fx.handoff);
    f
}

/// At most one model request every `gap` (`callora eval --rpm`). The wait happens before
/// a request is timed, so latency stays the model's own.
pub struct Pacer {
    gap: std::time::Duration,
    next: tokio::sync::Mutex<tokio::time::Instant>,
}

impl Pacer {
    pub fn new(gap: std::time::Duration) -> Self {
        Self { gap, next: tokio::sync::Mutex::new(tokio::time::Instant::now()) }
    }

    async fn wait_turn(&self) {
        let mut next = self.next.lock().await;
        tokio::time::sleep_until(*next).await;
        *next = tokio::time::Instant::now() + self.gap;
    }
}

// ---------------------------------------------------------------------------------------
// Report

fn percentile(sorted: &[u64], p: f64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted.get(i).copied()
}

#[derive(Debug, Serialize)]
pub struct ModelReport {
    pub model: String,
    pub runs: usize,
    pub passed: usize,
    pub pass_rate: f64,
    pub first_words_p50_ms: Option<u64>,
    pub first_words_p90_ms: Option<u64>,
    pub decision_p50_ms: Option<u64>,
    pub decision_p90_ms: Option<u64>,
    pub tokens: Usage,
    /// Dollars per model turn, when prices are known.
    pub cost_per_turn: Option<f64>,
    /// Case id → (passed, runs).
    pub cases: BTreeMap<String, (usize, usize)>,
    pub failed: Vec<CaseRun>,
}

pub fn summarize(model: &str, runs: Vec<CaseRun>, prices: &Prices) -> ModelReport {
    let mut first: Vec<u64> = Vec::new();
    let mut decision: Vec<u64> = Vec::new();
    let mut tokens = Usage { model: model.into(), ..Usage::default() };
    let mut dollars = 0.0;
    let mut priced = 0usize;
    let mut model_turns = 0usize;
    let mut cases: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for r in &runs {
        let entry = cases.entry(r.case.clone()).or_default();
        entry.1 += 1;
        if r.passed {
            entry.0 += 1;
        }
        for t in &r.turns {
            if t.route != Route::Agent {
                continue;
            }
            model_turns += 1;
            first.extend(t.first_words_ms);
            decision.extend(t.decision_ms);
            if let Some(u) = &t.usage {
                tokens.input += u.input;
                tokens.cached += u.cached;
                tokens.output += u.output;
                if let Some(c) = cost(u, prices) {
                    dollars += c;
                    priced += 1;
                }
            }
        }
    }
    first.sort_unstable();
    decision.sort_unstable();
    let passed = runs.iter().filter(|r| r.passed).count();
    ModelReport {
        model: model.into(),
        runs: runs.len(),
        passed,
        pass_rate: if runs.is_empty() { 0.0 } else { passed as f64 / runs.len() as f64 },
        first_words_p50_ms: percentile(&first, 0.5),
        first_words_p90_ms: percentile(&first, 0.9),
        decision_p50_ms: percentile(&decision, 0.5),
        decision_p90_ms: percentile(&decision, 0.9),
        tokens,
        cost_per_turn: (priced > 0 && priced == model_turns).then(|| dollars / priced as f64),
        cases,
        failed: runs.into_iter().filter(|r| !r.passed).collect(),
    }
}

/// The report as text for the terminal.
pub fn render(reports: &[ModelReport]) -> String {
    let mut s = String::new();
    let ms = |v: Option<u64>| v.map_or("-".to_string(), |v| format!("{v}"));
    for r in reports {
        s.push_str(&format!(
            "\n== {} ==  passed {}/{} ({:.0}%)  first words p50 {} ms, p90 {} ms  decision p50 {} ms, p90 {} ms\n",
            r.model,
            r.passed,
            r.runs,
            r.pass_rate * 100.0,
            ms(r.first_words_p50_ms),
            ms(r.first_words_p90_ms),
            ms(r.decision_p50_ms),
            ms(r.decision_p90_ms),
        ));
        s.push_str(&format!(
            "   tokens: {} in ({} cached), {} out{}\n",
            r.tokens.input,
            r.tokens.cached,
            r.tokens.output,
            r.cost_per_turn.map_or(String::new(), |c| format!("  ≈ ${c:.5} per turn")),
        ));
        for (case, (passed, runs)) in &r.cases {
            let mark = if passed == runs {
                "ok  "
            } else if *passed == 0 {
                "FAIL"
            } else {
                "FLAKY"
            };
            s.push_str(&format!("   {mark:<5} {passed}/{runs}  {case}\n"));
        }
        let mut shown = std::collections::HashSet::new();
        for run in &r.failed {
            // One failing run per case is enough to see what went wrong.
            if !shown.insert(run.case.clone()) {
                continue;
            }
            s.push_str(&format!("\n   ✗ {}\n", run.case));
            if let Some(e) = &run.error {
                s.push_str(&format!("       error: {e}\n"));
            }
            for (i, t) in run.turns.iter().enumerate() {
                if t.failures.is_empty() {
                    continue;
                }
                s.push_str(&format!("       turn {}: caller \"{}\"  →  \"{}\"\n", i + 1, t.caller, t.heard));
                for f in &t.failures {
                    s.push_str(&format!("         - {f}\n"));
                }
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use async_trait::async_trait;
    use parking_lot::Mutex;
    use serde_json::json;

    use callora_core::llm::LlmRequest;
    use callora_runtime::actions::ConfiguredActions;

    use super::*;

    /// A model that answers from a script, in order.
    struct Scripted(Mutex<Vec<Value>>);

    #[async_trait]
    impl LanguageModel for Scripted {
        async fn extract(&self, _request: &LlmRequest) -> anyhow::Result<Value> {
            let mut replies = self.0.lock();
            anyhow::ensure!(!replies.is_empty(), "the script ran out");
            Ok(replies.remove(0))
        }
        fn name(&self) -> &'static str {
            "scripted"
        }
    }

    fn root() -> PathBuf {
        PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
    }

    fn registry() -> Arc<BusinessRegistry> {
        let lookup = |k: &str| k.ends_with("HANDOFF_NUMBER").then(|| "+972500000001".to_string());
        Arc::new(BusinessRegistry::load_dir(&root().join("businesses"), &lookup).expect("the businesses load"))
    }

    fn runner() -> Runner {
        Runner {
            registry: registry(),
            gazetteer: None,
            actions: Arc::new(ConfiguredActions::new(reqwest::Client::new(), Default::default())),
            pacer: None,
        }
    }

    fn case(turns: Value) -> Case {
        serde_json::from_value(json!({ "id": "t", "turns": turns })).unwrap()
    }

    #[test]
    fn the_shipped_cases_are_well_formed() {
        let cases = load_cases(&root().join("evaluation/agent")).expect("the cases load");
        assert!(cases.len() >= 20, "{} cases", cases.len());
        let reg = registry();
        let problems: Vec<String> = cases.iter().flat_map(|c| check_case(c, &reg)).collect();
        assert!(problems.is_empty(), "{problems:#?}");
    }

    #[tokio::test]
    async fn a_case_passes_when_the_model_does_what_it_expects() {
        let c = case(json!([
            { "caller": "צריך מונית לתל אביב", "expect": {
                "action": ["none"], "task": "book_ride", "fields": { "destination": "תל אביב" },
                "no_fields": ["passengers"], "says_any": ["מאיפה|מאיזו עיר"], "step": "collecting" } }
        ]));
        let model = Scripted(Mutex::new(vec![json!({
            "action": "none", "task": "book_ride",
            "fields": [{ "slot": "destination", "value": "תל אביב" }], "say": "מאיזו עיר לאסוף?"
        })]));
        let run = runner().run_case(&c, &model, 7).await;
        assert!(run.passed, "{run:#?}");
        assert_eq!(run.turns[0].route, Route::Agent);
        assert!(run.turns[0].decision_ms.is_some());
    }

    #[tokio::test]
    async fn every_broken_expectation_is_reported() {
        let c = case(json!([
            { "caller": "אפשר לשים מונית?", "expect": {
                "task": "book_ride", "no_fields": ["destination"], "says_none": ["לא הבנתי"], "ended": false } }
        ]));
        let model = Scripted(Mutex::new(vec![json!({
            "action": "none", "task": null,
            "fields": [{ "slot": "destination", "value": "שים" }], "say": "לא הבנתי, אפשר שוב?"
        })]));
        let run = runner().run_case(&c, &model, 7).await;
        assert!(!run.passed);
        let f = &run.turns[0].failures;
        assert!(f.iter().any(|m| m.starts_with("task")), "{f:?}");
        assert!(f.iter().any(|m| m.contains("never gave it")), "{f:?}");
        assert!(f.iter().any(|m| m.contains("must not say")), "{f:?}");
    }

    #[tokio::test]
    async fn scripted_turns_set_the_scene_and_a_plain_yes_takes_the_fast_lane() {
        let scene = |caller: &str, fields: Value, action: &str, say: &str| json!({ "caller": caller, "scripted": { "action": action, "task": "book_ride", "fields": fields, "say": say } });
        let c = case(json!([
            scene("צריך מונית מהרצל 10 רעננה", json!([{ "slot": "pickup", "value": "הרצל 10, רעננה" }]), "none", "לאן נוסעים?"),
            scene("לעזריאלי", json!([{ "slot": "destination", "value": "עזריאלי" }]), "none", "כמה נוסעים?"),
            scene("שניים", json!([{ "slot": "passengers", "value": "2" }]), "none", "על שם מי ההזמנה?"),
            scene("דני", json!([{ "slot": "customer_name", "value": "דני" }]), "none", "יש משהו שהנהג צריך לדעת?"),
            scene("לא", json!([]), "read_back", "סגור."),
            { "caller": "כן", "expect": { "submitted": true, "slots": { "passengers": "2", "customer_name": "דני" } } }
        ]));
        let run = runner().run_case(&c, &Scripted(Mutex::new(Vec::new())), 3).await;
        assert!(run.passed, "{run:#?}");
        assert_eq!(run.turns.last().unwrap().route, Route::FastLane);
    }

    #[tokio::test]
    async fn a_model_error_fails_the_case_without_a_panic() {
        let c = case(json!([{ "caller": "צריך עזרה עם משהו", "expect": { "action": ["none"] } }]));
        let run = runner().run_case(&c, &Scripted(Mutex::new(Vec::new())), 1).await;
        assert!(!run.passed);
        assert!(run.error.as_deref().unwrap_or("").contains("ran out"));
    }

    #[test]
    fn the_report_lists_failures_once_per_case() {
        let failed = CaseRun {
            case: "x".into(),
            passed: false,
            turns: vec![TurnRun {
                caller: "היי".into(),
                route: Route::Agent,
                reply: None,
                heard: "לא הבנתי".into(),
                first_words_ms: Some(700),
                decision_ms: Some(900),
                usage: None,
                failures: vec!["heard …".into()],
            }],
            error: None,
        };
        let r = summarize("m", vec![failed.clone(), failed], &BTreeMap::new());
        assert_eq!(r.cases["x"], (0, 2));
        let text = render(&[r]);
        assert_eq!(text.matches("✗ x").count(), 1, "{text}");
        assert!(text.contains("FAIL"));
    }
}

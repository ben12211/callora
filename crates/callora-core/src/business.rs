//! Loading, validating and compiling a Business JSON into the form the runtime uses.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;

use once_cell_regex::placeholder_regex;
use regex::Regex;

use crate::config::*;
use crate::text::{normalize, PhraseSet};

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("{path}: cannot read: {source}")]
    Io { path: String, source: std::io::Error },
    #[error("{path}: invalid JSON: {source}")]
    Json { path: String, source: serde_json::Error },
    #[error("{path}: invalid business configuration:\n{}", format_issues(.issues))]
    Invalid { path: String, issues: Vec<Issue> },
    #[error("phone number {number} is claimed by both `{first}` and `{second}`")]
    DuplicateNumber { number: String, first: String, second: String },
    #[error("business id `{0}` is defined twice")]
    DuplicateId(String),
}

fn format_issues(issues: &[Issue]) -> String {
    issues.iter().map(|i| format!("  - {}: {}", i.path, i.message)).collect::<Vec<_>>().join("\n")
}

/// One validation problem, located by a JSON-ish path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub path: String,
    pub message: String,
}

mod once_cell_regex {
    use regex::Regex;
    use std::sync::OnceLock;

    pub fn placeholder_regex() -> &'static Regex {
        static RE: OnceLock<Regex> = OnceLock::new();
        RE.get_or_init(|| Regex::new(r"\{([a-z_][a-z0-9_]*)\}").expect("static regex"))
    }
}

/// Placeholders used in a response variant, in order.
pub fn placeholders(template: &str) -> Vec<String> {
    placeholder_regex().captures_iter(template).map(|c| c[1].to_string()).collect()
}

#[derive(Debug)]
pub struct CompiledMeta {
    pub intent: MetaIntent,
    pub phrases: PhraseSet,
    pub exact: PhraseSet,
    pub response: Option<ResponseId>,
}

#[derive(Debug)]
pub struct CompiledPlace {
    pub name: String,
    pub address: Option<String>,
    pub aliases: PhraseSet,
}

#[derive(Debug, Default)]
pub struct CompiledSlot {
    pub patterns: Vec<Regex>,
    pub enum_values: Vec<(String, PhraseSet)>,
    pub context_aliases: Vec<(PhraseSet, String)>,
    pub synonyms: Vec<(PhraseSet, serde_json::Value)>,
    pub strip_prefixes: Vec<String>,
}

/// A validated business with every matcher precompiled. Cheap to share across calls.
#[derive(Debug)]
pub struct Business {
    pub config: BusinessConfig,
    pub phone_numbers: Vec<String>,
    pub affirm: PhraseSet,
    pub deny: PhraseSet,
    pub fillers: PhraseSet,
    pub now: PhraseSet,
    pub meta: Vec<CompiledMeta>,
    pub intent_keywords: Vec<(String, PhraseSet)>,
    pub slots: BTreeMap<SlotId, CompiledSlot>,
    pub places: Vec<CompiledPlace>,
    pub voice_id: Option<String>,
    pub handoff_number: Option<String>,
    pub pronouncer: crate::speech::Pronouncer,
}

impl Business {
    pub fn intent(&self, id: &str) -> Option<&IntentConfig> {
        self.config.intents.iter().find(|i| i.id == id)
    }

    pub fn pipeline(&self, id: &str) -> Option<&PipelineConfig> {
        self.config.pipelines.get(id)
    }

    pub fn response(&self, id: &str) -> Option<&ResponseConfig> {
        self.config.responses.get(id)
    }

    pub fn meta(&self, intent: MetaIntent) -> Option<&CompiledMeta> {
        self.meta.iter().find(|m| m.intent == intent)
    }

    /// Parse, validate and compile. `env` resolves `*_env` references (pass
    /// `|k| std::env::var(k).ok()` in production).
    pub fn from_json(json: &str, source: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<Self, LoadError> {
        let config: BusinessConfig =
            serde_json::from_str(json).map_err(|e| LoadError::Json { path: source.to_string(), source: e })?;
        Self::compile(config, source, env)
    }

    pub fn compile(
        config: BusinessConfig,
        source: &str,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, LoadError> {
        let mut issues = validate(&config);
        let phrase_issues = std::cell::RefCell::new(Vec::new());
        let phrase = |path: String, items: &[String], prefixes: bool| -> PhraseSet {
            PhraseSet::new(items.iter(), prefixes).unwrap_or_else(|e| {
                phrase_issues.borrow_mut().push(Issue { path, message: format!("cannot compile phrase: {e}") });
                PhraseSet::default()
            })
        };

        let affirm = phrase("lexicon.affirm".into(), &config.lexicon.affirm, false);
        let deny = phrase("lexicon.deny".into(), &config.lexicon.deny, false);
        let fillers = phrase("lexicon.fillers".into(), &config.lexicon.fillers, false);
        let now = phrase("lexicon.now".into(), &config.lexicon.now, true);

        let mut meta = Vec::new();
        for (key, m) in &config.meta_intents {
            let Some(intent) = MetaIntent::parse(key) else { continue };
            meta.push(CompiledMeta {
                intent,
                phrases: phrase(format!("meta_intents.{key}.phrases"), &m.phrases, false),
                exact: phrase(format!("meta_intents.{key}.exact"), &m.exact, false),
                response: m.response.clone(),
            });
        }

        let intent_keywords = config
            .intents
            .iter()
            .map(|i| (i.id.clone(), phrase(format!("intents.{}.keywords", i.id), &i.keywords, true)))
            .collect();

        let mut slots = BTreeMap::new();
        for (id, s) in &config.slots {
            let mut compiled = CompiledSlot { strip_prefixes: s.strip_prefixes.clone(), ..Default::default() };
            for (n, p) in s.patterns.iter().enumerate() {
                match Regex::new(p) {
                    Ok(re) => compiled.patterns.push(re),
                    Err(e) => issues.push(Issue { path: format!("slots.{id}.patterns[{n}]"), message: e.to_string() }),
                }
            }
            for (canonical, phrases) in &s.values {
                compiled
                    .enum_values
                    .push((canonical.clone(), phrase(format!("slots.{id}.values.{canonical}"), phrases, true)));
            }
            for (phrase_text, value) in &s.synonyms {
                compiled.synonyms.push((
                    phrase(format!("slots.{id}.synonyms"), std::slice::from_ref(phrase_text), true),
                    value.clone(),
                ));
            }
            for (alias, key) in &s.context_aliases {
                compiled.context_aliases.push((
                    phrase(format!("slots.{id}.context_aliases"), std::slice::from_ref(alias), false),
                    key.clone(),
                ));
            }
            slots.insert(id.clone(), compiled);
        }

        let places = config
            .places
            .iter()
            .enumerate()
            .map(|(n, p)| {
                let mut all = p.aliases.clone();
                all.push(p.name.clone());
                CompiledPlace {
                    name: p.name.clone(),
                    address: p.address.clone(),
                    aliases: phrase(format!("places[{n}]"), &all, true),
                }
            })
            .collect();

        let mut phone_numbers = config.phone_numbers.clone();
        if let Some(var) = &config.phone_numbers_env {
            if let Some(value) = env(var) {
                phone_numbers.extend(value.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
            }
        }
        for (n, number) in phone_numbers.iter().enumerate() {
            if !is_e164(number) {
                issues.push(Issue { path: format!("phone_numbers[{n}]"), message: "must be an E.164 number".into() });
            }
        }

        let voice_id = config
            .voice
            .voice_id
            .clone()
            .or_else(|| config.voice.voice_id_env.as_deref().and_then(env))
            .filter(|v| !v.trim().is_empty());
        let handoff_number = config
            .handoff
            .phone_number_env
            .as_deref()
            .and_then(env)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if let Some(n) = &handoff_number {
            if !is_e164(n) {
                issues.push(Issue {
                    path: "handoff.phone_number_env".into(),
                    message: "resolves to a value that is not E.164".into(),
                });
            }
        }

        let pronouncer = crate::speech::Pronouncer::new(&config.pronunciations).unwrap_or_else(|e| {
            issues.push(Issue { path: "pronunciations".into(), message: format!("cannot compile: {e}") });
            crate::speech::Pronouncer::default()
        });
        issues.extend(phrase_issues.into_inner());
        if !issues.is_empty() {
            return Err(LoadError::Invalid { path: source.to_string(), issues });
        }
        Ok(Self {
            config,
            phone_numbers,
            affirm,
            deny,
            fillers,
            now,
            meta,
            intent_keywords,
            slots,
            places,
            voice_id,
            handoff_number,
            pronouncer,
        })
    }
}

pub fn is_e164(value: &str) -> bool {
    let Some(digits) = value.strip_prefix('+') else { return false };
    (8..=15).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit()) && !digits.starts_with('0')
}

/// Semantic validation: every cross-reference resolves and every value is in range.
pub fn validate(c: &BusinessConfig) -> Vec<Issue> {
    let mut v = Vec::new();
    let mut err = |path: &str, message: String| v.push(Issue { path: path.to_string(), message });

    if c.schema_version != SCHEMA_VERSION {
        err("schema_version", format!("unsupported version {}, expected {SCHEMA_VERSION}", c.schema_version));
    }
    if c.id.is_empty() || !c.id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    {
        err("id", "must match [a-z0-9_-]+".into());
    }
    for (field, value) in [("name", &c.name), ("language", &c.language), ("timezone", &c.timezone)] {
        if value.trim().is_empty() {
            err(field, "must not be empty".into());
        }
    }
    if c.voice.voice_id.is_none() && c.voice.voice_id_env.is_none() {
        err("voice", "set voice_id or voice_id_env".into());
    }
    if !c.voice.deliveries.contains_key("normal") {
        err("voice.deliveries", "a `normal` delivery is required".into());
    }
    let s = &c.voice.settings;
    for (name, value, lo, hi) in [
        ("stability", s.stability, 0.0, 1.0),
        ("similarity_boost", s.similarity_boost, 0.0, 1.0),
        ("style", s.style, 0.0, 1.0),
        ("speed", s.speed, 0.7, 1.2),
    ] {
        if !(lo..=hi).contains(&value) {
            err(&format!("voice.settings.{name}"), format!("must be within {lo}..={hi}"));
        }
    }
    if c.lexicon.affirm.is_empty() || c.lexicon.deny.is_empty() {
        err("lexicon", "affirm and deny must not be empty".into());
    }

    let responses: BTreeSet<&str> = c.responses.keys().map(String::as_str).collect();
    let need_response = |path: &str, id: &str, v: &mut Vec<Issue>| {
        if !responses.contains(id) {
            v.push(Issue { path: path.into(), message: format!("unknown response `{id}`") });
        }
    };
    need_response("greeting", &c.greeting, &mut v);
    need_response("anything_else", &c.anything_else, &mut v);
    need_response("goodbye", &c.goodbye, &mut v);
    need_response("confirm_slot", &c.confirm_slot, &mut v);
    if let Some(r) = c.responses.get(&c.confirm_slot) {
        if !r.variants.iter().all(|t| placeholders(t).iter().any(|p| p == "value")) {
            v.push(Issue { path: "confirm_slot".into(), message: "every variant must use {value}".into() });
        }
    }
    if let Some(a) = &c.acknowledgement {
        need_response("acknowledgement", a, &mut v);
    }
    if c.fallback.ladder.is_empty() {
        v.push(Issue { path: "fallback.ladder".into(), message: "needs at least one response".into() });
    }
    for (n, r) in c.fallback.ladder.iter().enumerate() {
        need_response(&format!("fallback.ladder[{n}]"), r, &mut v);
    }
    need_response("handoff.response", &c.handoff.response, &mut v);
    need_response("handoff.unavailable_response", &c.handoff.unavailable_response, &mut v);
    if let Some(r) = &c.silence.response {
        need_response("silence.response", r, &mut v);
    }
    if let Some(r) = &c.understanding.thinking_filler {
        need_response("understanding.thinking_filler", r, &mut v);
    }
    if !(0.0..=1.0).contains(&c.understanding.llm_below_coverage) {
        v.push(Issue { path: "understanding.llm_below_coverage".into(), message: "must be within 0..=1".into() });
    }

    for (key, m) in &c.meta_intents {
        let path = format!("meta_intents.{key}");
        if MetaIntent::parse(key).is_none() {
            v.push(Issue { path: path.clone(), message: "unknown meta intent".into() });
        }
        if m.phrases.is_empty() && m.exact.is_empty() {
            v.push(Issue { path: path.clone(), message: "needs phrases or exact".into() });
        }
        if let Some(r) = &m.response {
            need_response(&format!("{path}.response"), r, &mut v);
        }
    }
    for required in [MetaIntent::CancelCurrentFlow, MetaIntent::Goodbye] {
        if let Some(m) = c.meta_intents.get(required.as_str()) {
            if m.response.is_none() {
                v.push(Issue {
                    path: format!("meta_intents.{}", required.as_str()),
                    message: "needs a response".into(),
                });
            }
        }
    }

    let mut seen = BTreeSet::new();
    if c.intents.is_empty() {
        v.push(Issue { path: "intents".into(), message: "at least one intent is required".into() });
    }
    for (n, i) in c.intents.iter().enumerate() {
        let path = format!("intents[{n}]({})", i.id);
        if !seen.insert(i.id.as_str()) {
            v.push(Issue { path: path.clone(), message: "duplicate intent id".into() });
        }
        let targets = usize::from(i.pipeline.is_some()) + usize::from(i.respond.is_some()) + usize::from(i.handoff);
        if targets != 1 {
            v.push(Issue { path: path.clone(), message: "set exactly one of pipeline, respond, handoff".into() });
        }
        if let Some(p) = &i.pipeline {
            if !c.pipelines.contains_key(p) {
                v.push(Issue { path: format!("{path}.pipeline"), message: format!("unknown pipeline `{p}`") });
            }
        }
        if let Some(r) = &i.respond {
            need_response(&format!("{path}.respond"), r, &mut v);
        }
    }

    for (id, s) in &c.slots {
        let path = format!("slots.{id}");
        for (n, p) in s.patterns.iter().enumerate() {
            match Regex::new(p) {
                Ok(re) if re.capture_names().flatten().any(|name| name == "value") => {}
                Ok(_) => v.push(Issue {
                    path: format!("{path}.patterns[{n}]"),
                    message: "needs a named group `value`".into(),
                }),
                Err(e) => v.push(Issue { path: format!("{path}.patterns[{n}]"), message: e.to_string() }),
            }
        }
        if s.kind == SlotKind::Enum && s.values.is_empty() {
            v.push(Issue { path: format!("{path}.values"), message: "an enum slot needs values".into() });
        }
        if let (Some(lo), Some(hi)) = (s.min, s.max) {
            if lo > hi {
                v.push(Issue { path: path.clone(), message: "min is greater than max".into() });
            }
        }
        if !(0.0..=1.0).contains(&s.confirm_below)
            || !(0.0..=1.0).contains(&s.reject_below)
            || s.reject_below > s.confirm_below
        {
            v.push(Issue { path: path.clone(), message: "need 0 <= reject_below <= confirm_below <= 1".into() });
        }
    }
    let slot_exists = |id: &str| c.slots.contains_key(id);

    for (id, p) in &c.pipelines {
        let path = format!("pipelines.{id}");
        let mut names = BTreeSet::new();
        for (n, ps) in p.slots.iter().enumerate() {
            let sp = format!("{path}.slots[{n}]");
            if !slot_exists(&ps.slot) {
                v.push(Issue { path: sp.clone(), message: format!("unknown slot `{}`", ps.slot) });
            }
            if !names.insert(ps.slot.as_str()) {
                v.push(Issue { path: sp.clone(), message: "slot listed twice".into() });
            }
            if ps.required && ps.ask.is_none() && ps.default.is_none() {
                v.push(Issue { path: sp.clone(), message: "a required slot without a default needs `ask`".into() });
            }
            if let Some(a) = &ps.ask {
                need_response(&format!("{sp}.ask"), a, &mut v);
            }
        }
        if let Some(cf) = &p.confirm {
            need_response(&format!("{path}.confirm.response"), &cf.response, &mut v);
            need_response(&format!("{path}.confirm.ask_change"), &cf.ask_change, &mut v);
        }
        for (field, r) in [
            ("filler", &p.filler),
            ("on_success", &p.on_success),
            ("on_failure", &p.on_failure),
            ("on_complete", &p.on_complete),
        ] {
            if let Some(r) = r {
                need_response(&format!("{path}.{field}"), r, &mut v);
            }
        }
        match &p.action {
            Some(a) => {
                match c.actions.get(a) {
                    None => v.push(Issue { path: format!("{path}.action"), message: format!("unknown action `{a}`") }),
                    Some(action) if action.requires_confirmation && p.confirm.is_none() => v.push(Issue {
                        path: format!("{path}.confirm"),
                        message: format!("action `{a}` requires confirmation but the pipeline has no confirm step"),
                    }),
                    Some(_) => {}
                }
                if p.on_success.is_none() {
                    v.push(Issue {
                        path: format!("{path}.on_success"),
                        message: "a pipeline with an action needs on_success".into(),
                    });
                }
            }
            None if p.on_complete.is_none() => {
                v.push(Issue { path: path.clone(), message: "a pipeline without an action needs on_complete".into() })
            }
            None => {}
        }
    }

    for (id, a) in &c.actions {
        if a.backends.is_empty() {
            v.push(Issue { path: format!("actions.{id}.backends"), message: "needs at least one backend".into() });
        }
        if a.max_attempts == 0 {
            v.push(Issue { path: format!("actions.{id}.max_attempts"), message: "must be at least 1".into() });
        }
    }
    if let Some(cl) = &c.customer_lookup {
        if !c.actions.contains_key(&cl.action) {
            v.push(Issue { path: "customer_lookup.action".into(), message: format!("unknown action `{}`", cl.action) });
        }
        if let Some(r) = &cl.known_greeting {
            need_response("customer_lookup.known_greeting", r, &mut v);
        }
    }

    for (id, r) in &c.responses {
        let path = format!("responses.{id}");
        if r.variants.is_empty() || r.variants.iter().any(|t| t.trim().is_empty()) {
            v.push(Issue { path: format!("{path}.variants"), message: "needs non-empty variants".into() });
        }
        if let Some(d) = &r.delivery {
            if !c.voice.deliveries.contains_key(d) {
                v.push(Issue { path: format!("{path}.delivery"), message: format!("unknown delivery `{d}`") });
            }
        }
        if let Some(p) = &r.prefix {
            match c.responses.get(p) {
                None => v.push(Issue { path: format!("{path}.prefix"), message: format!("unknown response `{p}`") }),
                Some(pr) if pr.prefix.is_some() => v.push(Issue {
                    path: format!("{path}.prefix"),
                    message: "a prefix cannot itself have a prefix".into(),
                }),
                Some(_) => {}
            }
        }
        for (name, param) in &r.params {
            let range = match param {
                ParamConfig::Count { range, .. } | ParamConfig::Number { range, .. } => Some(range),
                _ => None,
            };
            if let Some([lo, hi]) = range {
                if lo > hi || hi - lo > 500 {
                    v.push(Issue {
                        path: format!("{path}.params.{name}"),
                        message: "range must be ordered and span at most 500".into(),
                    });
                }
            }
            if !r.variants.iter().any(|t| placeholders(t).iter().any(|p| p == name)) {
                v.push(Issue { path: format!("{path}.params.{name}"), message: "declared but never used".into() });
            }
        }
        let pregenerable_params = r.variants.iter().flat_map(|t| placeholders(t)).filter(|p| {
            matches!(
                r.params.get(p),
                Some(ParamConfig::Count { .. } | ParamConfig::Number { .. } | ParamConfig::Enum { .. })
            )
        });
        let mut distinct = BTreeSet::new();
        distinct.extend(pregenerable_params);
        if distinct.len() > 2 {
            v.push(Issue {
                path: path.clone(),
                message: "at most two pre-generated parameters per response (combinatorial library size)".into(),
            });
        }
    }

    for (n, rule) in c.rules.iter().enumerate() {
        let path = format!("rules[{n}]({})", rule.id);
        if !slot_exists(&rule.when.slot) {
            v.push(Issue { path: format!("{path}.when.slot"), message: format!("unknown slot `{}`", rule.when.slot) });
        }
        if let Some(p) = &rule.pipeline {
            if !c.pipelines.contains_key(p) {
                v.push(Issue { path: format!("{path}.pipeline"), message: format!("unknown pipeline `{p}`") });
            }
        }
        match &rule.then {
            RuleEffect::Reject { response } => need_response(&format!("{path}.then.response"), response, &mut v),
            RuleEffect::Handoff { response: Some(r), .. } => need_response(&format!("{path}.then.response"), r, &mut v),
            RuleEffect::Handoff { response: None, .. } => {}
            RuleEffect::Set { slot, .. } => {
                if !slot_exists(slot) {
                    v.push(Issue { path: format!("{path}.then.slot"), message: format!("unknown slot `{slot}`") });
                }
            }
        }
    }

    for (word, spoken) in &c.pronunciations {
        if normalize(word).is_empty() || spoken.trim().is_empty() {
            v.push(Issue { path: format!("pronunciations.{word}"), message: "empty entry".into() });
        }
    }
    v
}

/// Every business the process serves, routed by the dialled number.
#[derive(Debug, Default, Clone)]
pub struct BusinessRegistry {
    by_id: BTreeMap<String, Arc<Business>>,
    by_number: HashMap<String, Arc<Business>>,
}

impl BusinessRegistry {
    pub fn new(businesses: Vec<Business>) -> Result<Self, LoadError> {
        let mut reg = Self::default();
        for b in businesses {
            let b = Arc::new(b);
            if reg.by_id.insert(b.config.id.clone(), b.clone()).is_some() {
                return Err(LoadError::DuplicateId(b.config.id.clone()));
            }
            for number in &b.phone_numbers {
                if let Some(prev) = reg.by_number.insert(number.clone(), b.clone()) {
                    return Err(LoadError::DuplicateNumber {
                        number: number.clone(),
                        first: prev.config.id.clone(),
                        second: b.config.id.clone(),
                    });
                }
            }
        }
        Ok(reg)
    }

    /// Load every `*.json` file in a directory.
    pub fn load_dir(dir: &Path, env: &dyn Fn(&str) -> Option<String>) -> Result<Self, LoadError> {
        let read_dir =
            std::fs::read_dir(dir).map_err(|e| LoadError::Io { path: dir.display().to_string(), source: e })?;
        let mut paths: Vec<_> = read_dir
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        paths.sort();
        let mut out = Vec::new();
        for path in paths {
            let shown = path.display().to_string();
            let json = std::fs::read_to_string(&path).map_err(|e| LoadError::Io { path: shown.clone(), source: e })?;
            out.push(Business::from_json(&json, &shown, env)?);
        }
        Self::new(out)
    }

    pub fn by_number(&self, number: &str) -> Option<Arc<Business>> {
        self.by_number.get(number).cloned()
    }

    pub fn by_id(&self, id: &str) -> Option<Arc<Business>> {
        self.by_id.get(id).cloned()
    }

    pub fn all(&self) -> impl Iterator<Item = &Arc<Business>> {
        self.by_id.values()
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

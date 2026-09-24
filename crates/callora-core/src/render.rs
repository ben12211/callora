//! Turning response ids into speech plans, and enumerating what the voice library holds.
//!
//! A [`SpeechPlan`] is a list of segments. Each segment is one audio unit: the runtime
//! plays it from the pre-generated library when the exact text (in that delivery) was
//! pre-generated, and synthesizes it otherwise. Splitting a reply into "ack" + "content"
//! means the cached acknowledgement starts playing while the content is being synthesized.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::business::{placeholders, Business};
use crate::config::{ParamConfig, ResponseConfig};
use crate::customer::Customer;
use crate::hebrew::{count_phrase, number_words};
use crate::values::SlotValue;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentOrigin {
    /// A fixed sentence; always in the library.
    Static,
    /// A template whose values were all pre-generated; in the library.
    Template,
    /// Contains free content; needs dynamic TTS.
    Dynamic,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeechSegment {
    pub text: String,
    pub response_id: String,
    pub delivery: String,
    pub origin: SegmentOrigin,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpeechPlan {
    pub segments: Vec<SpeechSegment>,
    /// Extra loudness requested by the caller ("speak louder"), applied at playout.
    #[serde(default)]
    pub gain_db: f32,
}

impl SpeechPlan {
    pub fn empty() -> Self {
        Self { segments: Vec::new(), gain_db: 0.0 }
    }

    /// Free text (an agent's reply): one segment, played from the library when that exact
    /// wording was pre-recorded and synthesized otherwise.
    pub fn free(text: &str, delivery: &str, gain_db: f32) -> Self {
        Self {
            segments: vec![SpeechSegment {
                text: text.trim().to_string(),
                response_id: "agent".into(),
                delivery: delivery.to_string(),
                origin: SegmentOrigin::Dynamic,
            }],
            gain_db,
        }
    }

    pub fn then(mut self, other: SpeechPlan) -> Self {
        self.segments.extend(other.segments);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    pub fn text(&self) -> String {
        self.segments.iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join(" ")
    }

    pub fn response_ids(&self) -> Vec<&str> {
        self.segments.iter().map(|s| s.response_id.as_str()).collect()
    }
}

/// Values available to placeholders.
#[derive(Debug, Default)]
pub struct RenderContext<'a> {
    pub slots: BTreeMap<String, SlotValue>,
    pub result: Option<&'a serde_json::Value>,
    pub customer: Option<&'a Customer>,
    pub extra: BTreeMap<String, String>,
}

/// A placeholder's value before it is spoken.
enum Raw {
    Number(i64),
    Text(String),
    Slot(SlotValue),
}

impl RenderContext<'_> {
    fn lookup(&self, name: &str) -> Option<Raw> {
        if let Some(v) = self.extra.get(name) {
            return Some(Raw::Text(v.clone()));
        }
        if let Some(r) = self.result.and_then(|r| r.get(name)) {
            return match r {
                serde_json::Value::Number(n) => n.as_i64().map(Raw::Number).or_else(|| Some(Raw::Text(n.to_string()))),
                serde_json::Value::String(s) => Some(Raw::Text(s.clone())),
                serde_json::Value::Bool(b) => Some(Raw::Text(b.to_string())),
                _ => None,
            };
        }
        if let Some(v) = self.slots.get(name) {
            return Some(Raw::Slot(v.clone()));
        }
        let customer = self.customer?;
        if name == "customer_name" {
            return customer.name.clone().map(Raw::Text);
        }
        match customer.data.get(name)? {
            serde_json::Value::String(s) => Some(Raw::Text(s.clone())),
            serde_json::Value::Number(n) => n.as_i64().map(Raw::Number),
            _ => None,
        }
    }
}

fn param_domain(param: &ParamConfig) -> Option<Vec<String>> {
    match param {
        ParamConfig::Count { gender, singular, plural, range } => {
            Some((range[0]..=range[1]).map(|n| count_phrase(n, *gender, singular, plural)).collect())
        }
        ParamConfig::Number { gender, range } => {
            Some((range[0]..=range[1]).map(|n| number_words(n, *gender)).collect())
        }
        ParamConfig::Enum { values } => Some(values.values().cloned().collect()),
        ParamConfig::Time | ParamConfig::Text => None,
    }
}

/// Speak one value under its declared parameter kind. Returns the text and whether that
/// text is part of the pre-generated domain.
fn speak(raw: Raw, param: Option<&ParamConfig>) -> Option<(String, bool)> {
    let as_int = |raw: &Raw| -> Option<i64> {
        match raw {
            Raw::Number(n) => Some(*n),
            Raw::Slot(SlotValue::Integer { value }) => Some(*value),
            Raw::Text(t) => t.trim().parse().ok(),
            _ => None,
        }
    };
    match param {
        Some(ParamConfig::Count { gender, singular, plural, range }) => {
            let n = as_int(&raw)?;
            Some((count_phrase(n, *gender, singular, plural), (range[0]..=range[1]).contains(&n)))
        }
        Some(ParamConfig::Number { gender, range }) => {
            let n = as_int(&raw)?;
            Some((number_words(n, *gender), (range[0]..=range[1]).contains(&n)))
        }
        Some(ParamConfig::Enum { values }) => {
            let key = match raw {
                Raw::Slot(SlotValue::Enum { value }) => value,
                Raw::Text(t) => t,
                Raw::Number(n) => n.to_string(),
                Raw::Slot(v) => v.spoken(),
            };
            match values.get(&key) {
                Some(spoken) => Some((spoken.clone(), true)),
                None => Some((key, false)),
            }
        }
        Some(ParamConfig::Time) | Some(ParamConfig::Text) | None => Some((
            match raw {
                Raw::Number(n) => n.to_string(),
                Raw::Text(t) => t,
                Raw::Slot(v) => v.spoken(),
            },
            false,
        )),
    }
}

/// Render one variant. `None` when a placeholder has no value.
fn render_variant(
    template: &str,
    response: &ResponseConfig,
    ctx: &RenderContext<'_>,
) -> Option<(String, SegmentOrigin)> {
    let names = placeholders(template);
    if names.is_empty() {
        return Some((template.to_string(), SegmentOrigin::Static));
    }
    let mut text = template.to_string();
    let mut all_pregenerated = true;
    for name in names {
        let raw = ctx.lookup(&name)?;
        let (spoken, pregenerated) = speak(raw, response.params.get(&name))?;
        all_pregenerated &= pregenerated;
        text = text.replace(&format!("{{{name}}}"), &spoken);
    }
    Some((text, if all_pregenerated { SegmentOrigin::Template } else { SegmentOrigin::Dynamic }))
}

/// Chooses variants without repeating the previous choice for the same response.
pub trait VariantChooser {
    /// Index in `0..n`, avoiding `previous` when `n > 1`.
    fn choose(&mut self, response_id: &str, n: usize, previous: Option<usize>) -> usize;
}

/// Deterministic xorshift chooser, seeded per call.
#[derive(Debug, Clone)]
pub struct SeededChooser(pub u64);

impl VariantChooser for SeededChooser {
    fn choose(&mut self, _response_id: &str, n: usize, previous: Option<usize>) -> usize {
        if n <= 1 {
            return 0;
        }
        let mut x = self.0.max(1);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        let pick = (x % n as u64) as usize;
        match previous {
            Some(p) if p == pick => (pick + 1) % n,
            _ => pick,
        }
    }
}

pub struct Renderer<'a> {
    pub business: &'a Business,
    /// Overrides every segment's delivery (e.g. `slow` after "speak slower").
    pub delivery_override: Option<&'a str>,
    pub gain_db: f32,
}

impl Renderer<'_> {
    /// Render a response (and its prefix) to a plan. `last_variant` tracks choices per
    /// response id so variants rotate.
    pub fn render(
        &self,
        response_id: &str,
        ctx: &RenderContext<'_>,
        chooser: &mut dyn VariantChooser,
        last_variant: &mut BTreeMap<String, usize>,
    ) -> Option<SpeechPlan> {
        let response = self.business.response(response_id)?;
        let mut plan = SpeechPlan { segments: Vec::new(), gain_db: self.gain_db };
        if let Some(prefix) = &response.prefix {
            if let Some(p) = self.render(prefix, ctx, chooser, last_variant) {
                plan.segments.extend(p.segments);
            }
        }
        let n = response.variants.len();
        let first = chooser.choose(response_id, n, last_variant.get(response_id).copied());
        // Try the chosen variant first, then the others, until one has all its values.
        for offset in 0..n {
            let index = (first + offset) % n;
            if let Some((text, origin)) = render_variant(&response.variants[index], response, ctx) {
                last_variant.insert(response_id.to_string(), index);
                plan.segments.push(SpeechSegment {
                    text,
                    response_id: response_id.to_string(),
                    delivery: self.delivery_for(response),
                    origin,
                });
                return Some(plan);
            }
        }
        tracing::warn!(response_id, "no variant could be rendered: missing placeholder values");
        None
    }

    fn delivery_for(&self, response: &ResponseConfig) -> String {
        if let Some(d) = self.delivery_override {
            if self.business.config.voice.deliveries.contains_key(d) {
                return d.to_string();
            }
        }
        response.delivery.clone().unwrap_or_else(|| "normal".to_string())
    }
}

/// One entry of the pre-generated voice library.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LibraryEntry {
    pub response_id: String,
    pub delivery: String,
    pub text: String,
}

/// Everything the voice library should contain for a business: every static variant, and
/// every expansion of every template whose placeholders all have finite domains. Static
/// variants are also produced for each delivery marked `pregenerate`.
pub fn library_entries(b: &Business) -> Vec<LibraryEntry> {
    let mut out = BTreeSet::new();
    let extra_deliveries: Vec<&String> =
        b.config.voice.deliveries.iter().filter(|(_, d)| d.pregenerate).map(|(k, _)| k).collect();
    for (id, r) in &b.config.responses {
        let own = r.delivery.clone().unwrap_or_else(|| "normal".into());
        for variant in &r.variants {
            let names = placeholders(variant);
            let mut deliveries = vec![own.clone()];
            deliveries.extend(extra_deliveries.iter().map(|d| (*d).clone()));
            if names.is_empty() {
                for d in deliveries {
                    out.insert(LibraryEntry { response_id: id.clone(), delivery: d, text: variant.clone() });
                }
                continue;
            }
            let mut domains = Vec::new();
            let mut finite = true;
            let mut seen = BTreeSet::new();
            for name in &names {
                if !seen.insert(name.clone()) {
                    continue;
                }
                match r.params.get(name).and_then(param_domain) {
                    Some(d) => domains.push((name.clone(), d)),
                    None => {
                        finite = false;
                        break;
                    }
                }
            }
            if !finite {
                continue;
            }
            let mut texts = vec![variant.clone()];
            for (name, domain) in &domains {
                texts = texts
                    .iter()
                    .flat_map(|t| domain.iter().map(move |v| t.replace(&format!("{{{name}}}"), v)))
                    .collect();
            }
            for text in texts {
                out.insert(LibraryEntry { response_id: id.clone(), delivery: own.clone(), text });
            }
        }
    }
    out.into_iter().collect()
}

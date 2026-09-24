//! Understanding an utterance: meta intent, business intent, yes/no, and slot values.
//!
//! The fast path here is deterministic and runs in microseconds. It is driven entirely by
//! the business config (lexicons, keywords, slot patterns, places) plus the conversation
//! context (which slot was just asked for, whether a read-back is pending). When it cannot
//! explain enough of the utterance, the runtime asks the LLM ([`crate::llm`]) and merges
//! the two with [`merge`].

use serde::{Deserialize, Serialize};

use crate::business::Business;
use crate::config::{MetaIntent, SlotConfig, SlotKind};
use crate::customer::Customer;
use crate::hebrew::find_numbers;
use crate::text::{normalize, tokens, Coverage};
use crate::time::{parse_time, parse_time_span};
use crate::values::{Provenance, SlotFill, SlotValue};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Rules,
    Llm,
    Merged,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Understanding {
    pub transcript: String,
    #[serde(default)]
    pub meta: Option<MetaIntent>,
    #[serde(default)]
    pub intent: Option<IntentGuess>,
    /// `Some(true)` for yes, `Some(false)` for no.
    #[serde(default)]
    pub affirm: Option<bool>,
    #[serde(default)]
    pub slots: Vec<SlotFill>,
    /// The caller sounds annoyed or upset (LLM only); switches to a calmer delivery.
    #[serde(default)]
    pub frustrated: bool,
    /// Fraction of the utterance the fast path explained.
    pub coverage: f32,
    pub source: Source,
    /// Nothing but filler words ("תודה.", "אה"). Recognizers invent these on line noise,
    /// so the runtime does not treat them as the caller talking.
    #[serde(default)]
    pub noise: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntentGuess {
    pub id: String,
    pub confidence: f32,
}

impl Understanding {
    pub fn empty(transcript: &str) -> Self {
        Self {
            transcript: transcript.to_string(),
            meta: None,
            intent: None,
            affirm: None,
            slots: Vec::new(),
            frustrated: false,
            coverage: 0.0,
            source: Source::Rules,
            noise: false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.meta.is_none() && self.intent.is_none() && self.affirm.is_none() && self.slots.is_empty()
    }

    pub fn slot(&self, id: &str) -> Option<&SlotFill> {
        self.slots.iter().find(|s| s.slot == id)
    }
}

/// What the conversation is waiting for, which changes how a bare answer is read.
#[derive(Debug, Clone, Copy, Default)]
pub struct Context<'a> {
    pub active_pipeline: Option<&'a str>,
    pub awaiting_slot: Option<&'a str>,
    pub awaiting_confirmation: bool,
    pub customer: Option<&'a Customer>,
}

/// The fast path. Also reports whether the LLM should be consulted.
pub fn fast_path(b: &Business, ctx: &Context<'_>, transcript: &str) -> (Understanding, bool) {
    let norm = normalize(transcript);
    let mut u = Understanding::empty(transcript);
    if norm.is_empty() {
        return (u, false);
    }
    let mut cov = Coverage::new(&norm);
    cov.mark_words(&b.fillers);

    // Meta intents. A whole-utterance match ("מה?") is certain; a contained phrase ("תחזור
    // על זה בבקשה") counts too, but only the longest one wins.
    let mut meta_exact = false;
    let mut best_meta: Option<(MetaIntent, usize)> = None;
    for m in &b.meta {
        if m.exact.matches_whole(&norm, &b.fillers) {
            u.meta = Some(m.intent);
            meta_exact = true;
            cov.mark((0, norm.len()));
            break;
        }
        if let Some((p, span)) = m.phrases.find(&norm) {
            let len = p.text.chars().count();
            if best_meta.is_none_or(|(_, l)| len > l) {
                best_meta = Some((m.intent, len));
                cov.mark(span);
            }
        }
    }
    if u.meta.is_none() {
        u.meta = best_meta.map(|(m, _)| m);
    }

    // Yes / no: only as the opening words, so "לא" inside an address is not a refusal.
    let toks = tokens(&norm);
    let opening = toks.iter().take(3).copied().collect::<Vec<_>>().join(" ");
    let deny = b.deny.find(&opening);
    let affirm = b.affirm.find(&opening);
    match (affirm, deny) {
        (Some((_, a)), Some((_, d))) => {
            // Whichever comes first ("כן, לא משנה" is yes).
            if a.0 <= d.0 {
                u.affirm = Some(true);
                cov.mark(a);
            } else {
                u.affirm = Some(false);
                cov.mark(d);
            }
        }
        (Some((_, a)), None) => {
            u.affirm = Some(true);
            cov.mark(a);
        }
        (None, Some((_, d))) => {
            u.affirm = Some(false);
            cov.mark(d);
        }
        (None, None) => {}
    }
    // A meta phrase that is really a denial ("לא הבנתי") must not also count as "no".
    if u.meta.is_some() && u.affirm == Some(false) && meta_exact {
        u.affirm = None;
    }
    if u.meta.is_none() && u.affirm.is_none() && b.fillers.strip(&norm).is_empty() {
        u.noise = true;
        u.coverage = 1.0;
        return (u, false);
    }

    // Business intent by keywords: most hits wins; on a tie, the one mentioned first
    // (callers state the goal before the details: "השארתי תיק במונית").
    let mut best: Option<(String, usize, usize)> = None;
    for (id, keywords) in &b.intent_keywords {
        let hits = keywords.count(&norm);
        if hits == 0 {
            continue;
        }
        let first = keywords.first_position(&norm).unwrap_or(usize::MAX);
        if best.as_ref().is_none_or(|(_, h, f)| hits > *h || (hits == *h && first < *f)) {
            best = Some((id.clone(), hits, first));
        }
        cov.mark_words(keywords);
    }
    if let Some((id, hits, _)) = best {
        u.intent = Some(IntentGuess { id, confidence: (0.6 + 0.12 * hits as f32).min(0.9) });
    }

    // Slots, most specific first. Each pattern runs on the utterance with everything
    // explained so far masked out, and masks its own match for the slots after it.
    let mut order: Vec<(&String, &SlotConfig)> = b.config.slots.iter().collect();
    order.sort_by_key(|(id, cfg)| (cfg.priority, (*id).clone()));
    for (slot_id, cfg) in order {
        let Some(compiled) = b.slots.get(slot_id) else { continue };
        let in_scope = slot_in_scope(b, ctx, u.intent.as_ref(), slot_id);
        // Time expressions.
        if cfg.kind == SlotKind::Time && in_scope {
            if let Some((time, confidence, span)) = parse_time_span(&cov.masked(), &b.now) {
                cov.mark(span);
                push_fill(&mut u, slot_id, SlotValue::Time { time }, confidence, Provenance::Rules);
                continue;
            }
        }
        // Customer aliases ("מהבית").
        let masked = cov.masked();
        if let Some((span, value)) = compiled
            .context_aliases
            .iter()
            .find_map(|(phrases, key)| Some((phrases.find(&masked)?.1, customer_place(ctx.customer, key)?)))
        {
            cov.mark(span);
            push_fill(&mut u, slot_id, value, 0.9, Provenance::Customer);
            continue;
        }
        // Patterns run on the real text, so they can anchor on keywords ("השארתי ..."),
        // but a value stops at the first word something else already explained, and a
        // value that starts on one ("מונית" read as "from ונית") is skipped.
        let mut found = false;
        'patterns: for re in &compiled.patterns {
            let mut pos = 0;
            while pos <= norm.len() {
                let Some(caps) = re.captures_at(&norm, pos) else { break };
                let (Some(whole), Some(m)) = (caps.get(0), caps.name("value")) else { break };
                let mut end = m.start();
                while end < m.end() && !cov.is_covered(end) {
                    end += 1;
                }
                let raw = norm[m.start()..end].trim();
                if !raw.is_empty() && norm.is_char_boundary(end) {
                    if let Some((value, confidence)) = parse_slot_value(b, slot_id, cfg, raw, false, ctx.customer) {
                        cov.mark((whole.start(), end));
                        push_fill(&mut u, slot_id, value, confidence, Provenance::Rules);
                        found = true;
                        break 'patterns;
                    }
                }
                // Try again from the next character after this match's start.
                pos = whole.start() + 1;
                while pos < norm.len() && !norm.is_char_boundary(pos) {
                    pos += 1;
                }
            }
        }
        if found {
            continue;
        }
        // Synonyms ("לבד" → 1 passenger), only for slots of the pipeline in play.
        if in_scope {
            let masked = cov.masked();
            if let Some((span, value)) = compiled
                .synonyms
                .iter()
                .find_map(|(set, value)| Some((set.find(&masked)?.1, json_to_value(cfg, value)?)))
            {
                cov.mark(span);
                push_fill(&mut u, slot_id, value, 0.85, Provenance::Rules);
            }
        }
    }

    // A bare answer to the question just asked.
    let mut answered = false;
    // A doubtful value offered while one is being read back is usually a misheard "no,
    // <value>": worth the LLM, which knows what was just asked.
    let mut unsure_correction = false;
    if let (Some(slot_id), None) = (ctx.awaiting_slot, u.meta) {
        let switching =
            u.intent.as_ref().is_some_and(|i| Some(i.id.as_str()) != intent_of_pipeline(b, ctx.active_pipeline));
        if u.slot(slot_id).is_none() && !switching {
            if let Some(cfg) = b.config.slots.get(slot_id) {
                let answer = strip_opening_affirm(b, &norm);
                if let Some((value, confidence)) = parse_slot_value(b, slot_id, cfg, &answer, true, ctx.customer) {
                    push_fill(&mut u, slot_id, value, confidence, Provenance::Rules);
                    cov.mark((0, norm.len()));
                    answered = true;
                    unsure_correction = ctx.awaiting_confirmation && confidence < cfg.confirm_below;
                    // "כן" before an answer is not a confirmation of anything.
                    if cfg.kind != SlotKind::Boolean {
                        u.affirm = None;
                    }
                }
            }
        } else if u.slot(slot_id).is_some() {
            answered = true;
        }
    }

    u.coverage = cov.ratio();
    let threshold = b.config.understanding.llm_below_coverage;
    // Nothing understood at all: the LLM decides between "unclear" and "not meant for the
    // agent", instead of a reflexive "didn't catch that". A doubtful value the rules pulled
    // out of a sentence ("לשים" read as a destination) is checked by it too.
    let doubtful_value = u.slots.iter().any(|s| {
        s.provenance == Provenance::Rules && b.config.slots.get(&s.slot).is_some_and(|c| s.confidence < c.confirm_below)
    });
    let needs_llm =
        unsure_correction || doubtful_value || u.is_empty() || (!meta_exact && !answered && u.coverage < threshold);
    (u, needs_llm)
}

fn intent_of_pipeline<'a>(b: &'a Business, pipeline: Option<&str>) -> Option<&'a str> {
    let p = pipeline?;
    b.config.intents.iter().find(|i| i.pipeline.as_deref() == Some(p)).map(|i| i.id.as_str())
}

fn slot_in_scope(b: &Business, ctx: &Context<'_>, intent: Option<&IntentGuess>, slot_id: &str) -> bool {
    let pipeline =
        ctx.active_pipeline.or_else(|| intent.and_then(|i| b.intent(&i.id)).and_then(|i| i.pipeline.as_deref()));
    pipeline.and_then(|p| b.pipeline(p)).is_some_and(|p| p.slots.iter().any(|s| s.slot == slot_id))
}

fn strip_opening_affirm(b: &Business, norm: &str) -> String {
    match b.affirm.find(norm) {
        Some((_, (0, end))) => norm[end..].trim().to_string(),
        _ => norm.to_string(),
    }
}

fn push_fill(u: &mut Understanding, slot: &str, value: SlotValue, confidence: f32, provenance: Provenance) {
    if u.slot(slot).is_none() {
        u.slots.push(SlotFill { slot: slot.to_string(), value, confidence, provenance });
    }
}

fn customer_place(customer: Option<&Customer>, key: &str) -> Option<SlotValue> {
    let place = customer?.places.get(key)?;
    Some(SlotValue::Place {
        spoken: place.spoken.clone(),
        address: place.address.clone(),
        customer_place: Some(key.to_string()),
    })
}

fn json_to_value(cfg: &SlotConfig, value: &serde_json::Value) -> Option<SlotValue> {
    Some(match (cfg.kind, value) {
        (SlotKind::Integer, v) => SlotValue::Integer { value: v.as_i64()? },
        (SlotKind::Boolean, v) => SlotValue::Boolean { value: v.as_bool()? },
        (SlotKind::Enum, v) => SlotValue::Enum { value: v.as_str()?.to_string() },
        (SlotKind::Text, v) => SlotValue::Text { text: v.as_str()?.to_string() },
        (SlotKind::Place, v) => {
            SlotValue::Place { spoken: v.as_str()?.to_string(), address: None, customer_place: None }
        }
        (SlotKind::Time, v) => SlotValue::Time { time: serde_json::from_value(v.clone()).ok()? },
    })
}

/// Default values in pipeline configs are written the way a caller would say them.
pub fn default_value(b: &Business, slot_id: &str, value: &serde_json::Value) -> Option<SlotValue> {
    let cfg = b.config.slots.get(slot_id)?;
    if let Some(text) = value.as_str() {
        if cfg.kind == SlotKind::Time && text == "now" {
            return Some(SlotValue::Time { time: crate::time::TimeSpec::Now });
        }
        if let Some((v, _)) = parse_slot_value(b, slot_id, cfg, &normalize(text), true, None) {
            return Some(v);
        }
    }
    json_to_value(cfg, value)
}

/// Interpret a piece of text as a value for a slot. `bare` means the text is the caller's
/// whole answer to a question about this slot (so prefixes may be stripped and anything
/// goes for text slots). Returns `None` when the text is not a value of this kind.
pub fn parse_slot_value(
    b: &Business,
    slot_id: &str,
    cfg: &SlotConfig,
    text: &str,
    bare: bool,
    customer: Option<&Customer>,
) -> Option<(SlotValue, f32)> {
    let norm = normalize(text);
    let norm = b.fillers.strip(&norm);
    if norm.is_empty() {
        return None;
    }
    let compiled = b.slots.get(slot_id);
    if let Some(c) = compiled {
        for (phrases, key) in &c.context_aliases {
            if phrases.find(&norm).is_some() {
                if let Some(v) = customer_place(customer, key) {
                    return Some((v, 0.9));
                }
            }
        }
        for (set, value) in &c.synonyms {
            if set.matches_whole(&norm, &b.fillers) {
                if let Some(v) = json_to_value(cfg, value) {
                    return Some((v, 0.9));
                }
            }
        }
    }
    match cfg.kind {
        SlotKind::Integer => {
            let toks = tokens(&norm);
            let n = find_numbers(&toks).first().copied()?;
            let in_range = cfg.min.is_none_or(|lo| n.value >= lo) && cfg.max.is_none_or(|hi| n.value <= hi);
            let confidence = if in_range {
                if bare {
                    0.9
                } else {
                    0.85
                }
            } else {
                0.2
            };
            Some((SlotValue::Integer { value: n.value }, confidence))
        }
        SlotKind::Boolean => {
            if b.affirm.find(&norm).is_some_and(|(_, s)| s.0 == 0) {
                Some((SlotValue::Boolean { value: true }, 0.9))
            } else if b.deny.find(&norm).is_some_and(|(_, s)| s.0 == 0) {
                Some((SlotValue::Boolean { value: false }, 0.9))
            } else if !bare {
                // A pattern matched ("עם כיסא גלגלים"): the mention itself means yes.
                Some((SlotValue::Boolean { value: true }, 0.8))
            } else {
                None
            }
        }
        SlotKind::Time => parse_time(&norm, &b.now).map(|(time, c)| (SlotValue::Time { time }, c)),
        SlotKind::Enum => compiled?
            .enum_values
            .iter()
            .find(|(_, set)| set.find(&norm).is_some())
            .map(|(canonical, _)| (SlotValue::Enum { value: canonical.clone() }, 0.85)),
        SlotKind::Text => Some((SlotValue::Text { text: norm.clone() }, if bare { 0.8 } else { 0.75 })),
        SlotKind::Place => Some(parse_place(b, compiled, &norm, bare)),
    }
}

fn parse_place(
    b: &Business,
    compiled: Option<&crate::business::CompiledSlot>,
    norm: &str,
    bare: bool,
) -> (SlotValue, f32) {
    let gazetteer = |text: &str| -> Option<(SlotValue, f32)> {
        b.places.iter().find(|p| p.aliases.matches_whole(text, &b.fillers)).map(|p| {
            (SlotValue::Place { spoken: p.name.clone(), address: p.address.clone(), customer_place: None }, 0.95)
        })
    };
    if let Some(hit) = gazetteer(norm) {
        return hit;
    }
    let mut candidate = norm.to_string();
    if bare {
        if let Some(c) = compiled {
            for prefix in &c.strip_prefixes {
                if let Some(rest) = norm.strip_prefix(prefix.as_str()) {
                    if let Some(hit) = gazetteer(rest) {
                        return hit;
                    }
                    if rest.chars().count() >= 2 && !rest.starts_with(' ') {
                        candidate = rest.to_string();
                        break;
                    }
                }
            }
        }
    }
    // A place mentioned inside a longer phrase ("תחנת רכבת בני ברק" contains a known place).
    let contained = b.places.iter().find(|p| p.aliases.find(&candidate).is_some());
    let has_number = candidate.split(' ').any(|t| t.chars().any(|c| c.is_ascii_digit()));
    let confidence = match (contained, has_number) {
        (Some(_), _) => 0.85,
        (None, true) => 0.85,
        (None, false) if candidate.split(' ').count() >= 2 => 0.75,
        _ => 0.55,
    };
    (SlotValue::Place { spoken: candidate, address: None, customer_place: None }, confidence)
}

/// Below this, a rules value is only a guess (a lone word after a preposition).
const DOUBTFUL: f32 = 0.6;

/// Combine the fast path with an LLM result. Deterministic high-confidence findings are
/// kept; the LLM fills gaps and wins on low-confidence ones.
pub fn merge(fast: Understanding, llm: Understanding) -> Understanding {
    let mut out = fast.clone();
    out.source = Source::Merged;
    if out.meta.is_none() {
        out.meta = llm.meta;
    }
    match (&fast.intent, &llm.intent) {
        (None, Some(l)) => out.intent = Some(l.clone()),
        (Some(f), Some(l)) if l.confidence > f.confidence => out.intent = Some(l.clone()),
        _ => {}
    }
    if out.affirm.is_none() {
        out.affirm = llm.affirm;
    }
    out.frustrated = llm.frustrated;
    // A doubtful rules value the LLM did not see in the sentence was a misreading.
    out.slots.retain(|s| {
        s.provenance != Provenance::Rules || s.confidence >= DOUBTFUL || llm.slots.iter().any(|l| l.slot == s.slot)
    });
    for l in llm.slots {
        match out.slots.iter_mut().find(|s| s.slot == l.slot) {
            None => out.slots.push(l),
            Some(existing) if existing.confidence < 0.9 && l.confidence >= existing.confidence => *existing = l,
            Some(_) => {}
        }
    }
    // A yes/no the rules read off the first word of garbage ("לא רגעתיים...") does not
    // outweigh the LLM saying it was not meant for the agent.
    out.noise = llm.noise && out.meta.is_none() && out.intent.is_none() && out.slots.is_empty();
    if out.noise {
        out.affirm = None;
    }
    out.coverage = 1.0;
    out
}

#[cfg(test)]
mod tests {
    // Covered end to end against the taxi business in `tests/taxi.rs`.
}

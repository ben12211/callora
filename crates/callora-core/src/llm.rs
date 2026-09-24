//! The LLM half of understanding: a structured-extraction request built from the business
//! config and the live call state, and a strict parser for the reply.
//!
//! The LLM never decides what to *do* and never talks to the caller. It only answers the
//! same question the fast path answers (meta intent, intent, yes/no, slot values), and its
//! values go through the same typed parsers, so an LLM cannot put a value into the state
//! that the rules would not accept.

use serde_json::{json, Value};

use crate::business::Business;
use crate::config::{MetaIntent, SlotKind};
use crate::state::{CallState, Speaker};
use crate::understanding::{parse_slot_value, Context, IntentGuess, Source, Understanding};
use crate::values::{Provenance, SlotFill};

#[derive(Debug, Clone, PartialEq)]
pub struct LlmRequest {
    pub system: String,
    pub user: String,
    /// JSON Schema for the reply (OpenAI `json_schema` strict mode compatible).
    pub schema: Value,
}

fn kind_name(kind: SlotKind) -> &'static str {
    match kind {
        SlotKind::Text => "free text",
        SlotKind::Place => "a place or address, exactly as the caller said it",
        SlotKind::Integer => "a whole number",
        SlotKind::Boolean => "yes/no",
        SlotKind::Time => "a time as the caller said it (e.g. 'עכשיו', 'בעוד 10 דקות', '08:30')",
        SlotKind::Enum => "one of the listed values",
    }
}

fn meta_meaning(m: MetaIntent) -> &'static str {
    match m {
        MetaIntent::RepeatLast => "asks the agent to repeat what it just said",
        MetaIntent::DidNotUnderstand => "says they did not understand or hear",
        MetaIntent::SpeakSlower => "asks the agent to speak slower",
        MetaIntent::SpeakLouder => "asks the agent to speak louder",
        MetaIntent::CancelCurrentFlow => "drops what they were doing ('forget it')",
        MetaIntent::GoBack => "says they made a mistake in their last answer",
        MetaIntent::TransferHuman => "asks for a human",
        MetaIntent::Goodbye => "is ending the call",
        MetaIntent::Wait => "asks the agent to wait a moment, with nothing else",
    }
}

pub fn build_request(b: &Business, ctx: &Context<'_>, state: &CallState, transcript: &str) -> LlmRequest {
    let c = &b.config;
    let mut system = format!(
        "You are the language-understanding component of a phone agent for \"{}\" (language: {}). \
         You never reply to the caller. You only extract structured meaning from the caller's latest utterance, \
         which comes from speech recognition and may contain recognition errors.\n\n",
        c.name, c.language
    );
    system.push_str("Business intents (what the caller may want):\n");
    for i in &c.intents {
        system.push_str(&format!("- {}: {}", i.id, i.description));
        if !i.examples.is_empty() {
            system.push_str(&format!(
                " (e.g. {})",
                i.examples.iter().take(2).map(|e| format!("\"{e}\"")).collect::<Vec<_>>().join(", ")
            ));
        }
        system.push('\n');
    }
    system.push_str("\nConversation meta intents (about the conversation itself, not the business):\n");
    for m in MetaIntent::ALL {
        system.push_str(&format!("- {}: the caller {}\n", m.as_str(), meta_meaning(m)));
    }
    system.push_str("\nInformation slots:\n");
    for (id, s) in &c.slots {
        system.push_str(&format!("- {id}: {} ({})", s.description, kind_name(s.kind)));
        if s.kind == SlotKind::Enum {
            system.push_str(&format!(" values: {}", s.values.keys().cloned().collect::<Vec<_>>().join(", ")));
        }
        system.push('\n');
    }
    if !c.places.is_empty() {
        system.push_str("\nKnown places: ");
        system.push_str(&c.places.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", "));
        system.push('\n');
    }
    system.push_str(
        "\nRules:\n\
         - Extract only what the caller actually said in the latest utterance. Never invent or complete values.\n\
         - Slot values must be copied in the caller's own words (Hebrew stays Hebrew), without prepositions such as מ/ל/ב at the start of places.\n\
         - intent is null unless the caller expresses a business need. Answering a question the agent asked is not a new intent.\n\
         - affirm is true/false only when the caller answers yes/no to a question; otherwise null.\n\
         - Confidence is your probability (0..1) that the value is exactly right.\n\
         - frustrated is true only if the caller sounds clearly annoyed or upset.\n\
         - speech is \"not_for_agent\" when the utterance is not a reply to the agent: background talk, line noise, \
         or recognition garbage such as a lone \"תודה רבה\" or repeated syllables. It is \"unclear\" when the caller \
         clearly tried to say something that cannot be understood, and \"clear\" otherwise. Greetings and small talk \
         (\"מה המצב?\", \"היי\") are addressed to the agent: \"clear\", with the small-talk intent if there is one.\n\
         - A word after a preposition is a place only if it really names a place: in \"אני רוצה לשים מונית\" there is \
         no destination.\n",
    );

    let mut user = String::new();
    if let Some(run) = &state.run {
        user.push_str(&format!("Active flow: {} (intent {}).\n", run.pipeline, run.intent));
        if !run.slots.is_empty() {
            user.push_str("Already known:\n");
            for (k, v) in &run.slots {
                user.push_str(&format!("- {k}: {}\n", v.value.spoken()));
            }
        }
    }
    if let Some(slot) = ctx.awaiting_slot {
        user.push_str(&format!("The agent just asked the caller for: {slot}.\n"));
    }
    if ctx.awaiting_confirmation {
        user.push_str("The agent just read details back and asked the caller to confirm (yes/no, or a correction).\n");
    }
    let recent: Vec<String> = state
        .history
        .iter()
        .rev()
        .take(4)
        .rev()
        .map(|t| format!("{}: {}", if t.speaker == Speaker::Agent { "Agent" } else { "Caller" }, t.text))
        .collect();
    if !recent.is_empty() {
        user.push_str(&format!("Recent turns:\n{}\n", recent.join("\n")));
    }
    user.push_str(&format!("\nCaller's latest utterance: \"{transcript}\""));

    let intents: Vec<Value> = c.intents.iter().map(|i| json!(i.id)).chain([Value::Null]).collect();
    let metas: Vec<Value> = MetaIntent::ALL.iter().map(|m| json!(m.as_str())).chain([Value::Null]).collect();
    let slots: Vec<Value> = c.slots.keys().map(|k| json!(k)).collect();
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["speech", "meta_intent", "intent", "intent_confidence", "affirm", "frustrated", "slots"],
        "properties": {
            "speech": { "type": "string", "enum": ["clear", "unclear", "not_for_agent"] },
            "meta_intent": { "type": ["string", "null"], "enum": metas },
            "intent": { "type": ["string", "null"], "enum": intents },
            "intent_confidence": { "type": "number" },
            "affirm": { "type": ["boolean", "null"] },
            "frustrated": { "type": "boolean" },
            "slots": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["slot", "value", "confidence"],
                    "properties": {
                        "slot": { "type": "string", "enum": slots },
                        "value": { "type": "string" },
                        "confidence": { "type": "number" }
                    }
                }
            }
        }
    });
    LlmRequest { system, user, schema }
}

/// Parse and validate the LLM's JSON. Anything that does not fit the business is dropped.
pub fn parse_response(b: &Business, ctx: &Context<'_>, transcript: &str, reply: &Value) -> Understanding {
    let mut u = Understanding::empty(transcript);
    u.source = Source::Llm;
    u.coverage = 1.0;
    u.meta = reply.get("meta_intent").and_then(Value::as_str).and_then(MetaIntent::parse);
    if let Some(id) = reply.get("intent").and_then(Value::as_str) {
        if b.intent(id).is_some() {
            let confidence =
                reply.get("intent_confidence").and_then(Value::as_f64).unwrap_or(0.7).clamp(0.0, 1.0) as f32;
            u.intent = Some(IntentGuess { id: id.to_string(), confidence });
        }
    }
    u.affirm = reply.get("affirm").and_then(Value::as_bool);
    u.frustrated = reply.get("frustrated").and_then(Value::as_bool).unwrap_or(false);
    let not_for_agent = reply.get("speech").and_then(Value::as_str) == Some("not_for_agent");
    for item in reply.get("slots").and_then(Value::as_array).into_iter().flatten() {
        let (Some(slot), Some(value)) =
            (item.get("slot").and_then(Value::as_str), item.get("value").and_then(Value::as_str))
        else {
            continue;
        };
        let Some(cfg) = b.config.slots.get(slot) else { continue };
        let llm_confidence = item.get("confidence").and_then(Value::as_f64).unwrap_or(0.6).clamp(0.0, 1.0) as f32;
        if let Some((value, parsed_confidence)) = parse_slot_value(b, slot, cfg, value, true, ctx.customer) {
            if u.slot(slot).is_none() {
                u.slots.push(SlotFill {
                    slot: slot.to_string(),
                    value,
                    confidence: llm_confidence.min(parsed_confidence.max(0.6)),
                    provenance: Provenance::Llm,
                });
            }
        }
    }
    u.noise = not_for_agent && u.is_empty();
    u
}

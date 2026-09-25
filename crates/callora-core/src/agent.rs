//! The LLM agent: the model runs the conversation, the engine keeps it safe.
//!
//! Rules and templates cannot cover every way a caller talks; live calls kept hitting the
//! gaps ("מה המצב?" met with silence, "מה אמרת?" taken for goodbye). With an agent, every
//! turn the model sees the whole conversation and the booking so far and decides what to
//! say and what to do next. It never acts on its own: its decision is a small JSON object
//! ([`AgentTurn`]) that [`crate::engine::Engine::on_agent_turn`] applies under the hard
//! rules: values go through the same typed parsers as the fast path, nothing is sent
//! without a read-back and a yes, and the call ends only on a real goodbye.
//!
//! Speed: the reply opens with `action` (a few tokens) and then `say`, so the runtime starts
//! speaking while the model is still writing the rest ([`SayStream`]), yet knows first
//! whether the turn reads back or submits (those words wait for the engine). The model is
//! offered the business's pre-recorded phrases, which play instantly when used verbatim.
//! Measured with gpt-4.1 on this prompt: first words ~620 ms after the request.

use serde_json::{json, Value};

use crate::business::Business;
use crate::config::SlotKind;
use crate::llm::LlmRequest;
use crate::state::{CallState, Speaker, Step};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentAction {
    /// Just talk.
    None,
    /// Read the task's details back and wait for yes/no.
    ReadBack,
    /// Run the task's action; only honoured right after a confirmed read-back.
    Submit,
    /// Hand the caller to a human.
    Transfer,
    /// Hang up; only honoured when the caller really said goodbye.
    EndCall,
}

impl AgentAction {
    const ALL: [(&'static str, AgentAction); 5] = [
        ("none", AgentAction::None),
        ("read_back", AgentAction::ReadBack),
        ("submit", AgentAction::Submit),
        ("transfer", AgentAction::Transfer),
        ("end_call", AgentAction::EndCall),
    ];

    pub fn parse(s: &str) -> Self {
        Self::ALL.iter().find(|(n, _)| *n == s).map_or(AgentAction::None, |(_, a)| *a)
    }
}

/// One decision of the agent.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentTurn {
    pub say: String,
    pub action: AgentAction,
    /// Intent id the caller is on now, if any.
    pub task: Option<String>,
    /// (slot, value in the caller's words) given in this utterance.
    pub fields: Vec<(String, String)>,
}

/// The fixed wording of the business's instant phrases.
pub fn phrases(b: &Business) -> Vec<String> {
    let Some(agent) = &b.config.agent else { return Vec::new() };
    agent.phrases.iter().filter_map(|id| b.config.responses.get(id)).flat_map(|r| r.variants.iter().cloned()).collect()
}

fn kind_hint(kind: SlotKind) -> &'static str {
    match kind {
        SlotKind::Place => "place or address",
        SlotKind::Integer => "number",
        SlotKind::Time => "time, as said (e.g. עכשיו, בעוד 10 דקות, מחר ב-8)",
        SlotKind::Boolean => "yes/no",
        SlotKind::Enum => "one of the listed values",
        SlotKind::Text => "free text",
    }
}

/// The system prompt: who the agent is, what the business can do, and the rules. It does
/// not change during a call, so providers can cache it.
pub fn system_prompt(b: &Business) -> String {
    let c = &b.config;
    let agent = c.agent.as_ref();
    let mut s = String::new();
    if let Some(a) = agent {
        s.push_str(&a.persona);
        s.push_str("\n\n");
    }
    s.push_str(&format!(
        "You are the phone agent of \"{}\" (language {}). You run the conversation like an experienced human \
         dispatcher: listen, understand what the caller means, and move them to what they need in as few turns as \
         possible. The system executes your decisions and enforces the rules below.\n\n",
        c.name, c.language
    ));

    s.push_str("TASKS (task ids):\n");
    for i in &c.intents {
        s.push_str(&format!("- {}: {}", i.id, i.description));
        if let Some(p) = i.pipeline.as_deref().and_then(|p| b.pipeline(p)) {
            let details: Vec<String> = p
                .slots
                .iter()
                .map(|ps| match (&ps.default, ps.required) {
                    (Some(d), _) => format!("{} (default {}, do not ask)", ps.slot, d.as_str().unwrap_or("set")),
                    (None, true) => format!("{} (required)", ps.slot),
                    (None, false) => ps.slot.clone(),
                })
                .collect();
            if !details.is_empty() {
                s.push_str(&format!(". Details: {}", details.join(", ")));
            }
            if p.confirm.is_some() {
                s.push_str(". Needs read_back then submit");
            } else if p.action.is_some() {
                s.push_str(". Use submit once the required details are known");
            }
        } else if let Some(r) = i.respond.as_deref().and_then(|r| c.responses.get(r)) {
            if let Some(answer) = r.variants.first() {
                s.push_str(&format!(". Answer: \"{answer}\""));
            }
        } else if i.handoff {
            s.push_str(". Transfer to a human");
        }
        s.push('\n');
    }

    s.push_str("\nDETAILS:\n");
    for (id, slot) in &c.slots {
        s.push_str(&format!("- {id}: {} ({})", slot.description, kind_hint(slot.kind)));
        if slot.kind == SlotKind::Enum {
            s.push_str(&format!(": {}", slot.values.keys().cloned().collect::<Vec<_>>().join(", ")));
        }
        s.push('\n');
    }
    if !c.places.is_empty() {
        s.push_str(&format!(
            "Known places: {}\n",
            c.places.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }

    s.push_str(
        "\nREPLY with JSON, every turn:\n\
         - say: what you say now: exactly ONE short, natural sentence in the caller's language, like a real \
         dispatcher. Never repeat the greeting, never list options unless the caller is lost, never say you did \
         not understand and then ask something else in the same turn. Unless the caller is saying goodbye, it \
         moves the call on: after taking a detail it asks the next question (\"כמה נוסעים?\"), never just \
         \"הבנתי.\".\n\
         - action: \"none\"; \"read_back\" when every required detail of the task is known and the caller has \
         nothing to add: the system then reads the details back and asks to confirm, so your say is only \"סגור.\" \
         or \"אוקיי.\" (never a question); \"submit\" only when the caller just confirmed that read-back: the \
         system then sends it and tells the caller the result, so your say is only \"סגור.\"; \"transfer\" for a \
         human; \"end_call\" only when the caller clearly says goodbye or that they need nothing more.\n\
         - task: the task the caller is on now, or null.\n\
         - fields: details of the current task that the caller has given and that CURRENT TASK does not show \
         yet (from this utterance or an earlier one), copied in their words, without a leading preposition \
         (מ/ל/ב). A detail you mention or confirm must be in CURRENT TASK or in your fields; otherwise the \
         system does not have it. Never invent or complete a value. A word after a preposition is a place only if it \
         names a place (\"לשים מונית\" has no destination). A place is street, number and city when the \
         caller gave them (\"דיזנגוף 50, תל אביב\"), with the city from earlier in the call if they said it then; \
         the system checks it against Israel's official list of localities and streets.\n\
         \nRULES:\n\
         - Speech recognition makes mistakes. If the words make no sense, say you did not catch it and ask again \
         (mention what you did understand). Never guess a value, never end the call because of it.\n\
         - Greetings and small talk get a short friendly answer, then offer help.\n\
         - Prices, arrival times and availability come only from the system. Never promise them yourself.\n\
         - Ask for one missing required detail at a time, the most important first. Never ask about optional \
         details (luggage, seats, vehicle, notes) unless the caller brings them up.\n\
         - If the caller corrects a detail while details are being confirmed, take the correction and choose \
         read_back again.\n",
    );
    if let Some(a) = agent {
        for r in &a.rules {
            s.push_str(&format!("- {r}\n"));
        }
    }
    let instant = phrases(b);
    if !instant.is_empty() {
        s.push_str(
            "\nINSTANT PHRASES: pre-recorded, they play with no delay, while any other wording takes the voice about \
             a second to produce. Whenever one of them says what you mean, your say must be exactly that phrase:\n",
        );
        for p in &instant {
            s.push_str(&format!("\"{p}\"\n"));
        }
    }
    s
}

/// The per-turn message: the conversation so far, where the task stands, what was just said.
pub fn turn_message(b: &Business, state: &CallState, transcript: &str) -> String {
    let mut u = String::from("CONVERSATION:\n");
    for t in &state.history {
        let who = if t.speaker == Speaker::Agent { "Agent" } else { "Caller" };
        u.push_str(&format!("{who}: {}\n", t.text));
    }
    if let Some(name) = state.customer.as_ref().and_then(|c| c.name.as_deref()) {
        u.push_str(&format!("\nThe caller is a known customer: {name}.\n"));
    }
    for done in &state.completed {
        u.push_str(&format!("\nEarlier in this call: {} ({})", done.pipeline, done.outcome));
        if let Some(r) = &done.result {
            u.push_str(&format!(", result {r}"));
        }
        u.push('\n');
    }
    match &state.run {
        Some(run) => {
            u.push_str(&format!("\nCURRENT TASK: {}\n", run.intent));
            if let Some(p) = b.pipeline(&run.pipeline) {
                for ps in &p.slots {
                    match run.slots.get(&ps.slot) {
                        Some(v) => u.push_str(&format!("- {}: {}\n", ps.slot, v.value.spoken())),
                        None if state.place_cities.contains_key(&ps.slot) => u.push_str(&format!(
                            "- {}: city {}, street MISSING (required)\n",
                            ps.slot, state.place_cities[&ps.slot]
                        )),
                        None if ps.required => u.push_str(&format!("- {}: MISSING (required)\n", ps.slot)),
                        None => u.push_str(&format!("- {}: not given (optional)\n", ps.slot)),
                    }
                }
            }
            let status = match run.step {
                Step::AwaitingConfirmation => "details were read back; waiting for the caller's yes/no",
                Step::Executing { .. } => "being processed by the system",
                _ => "collecting details",
            };
            u.push_str(&format!("Status: {status}\n"));
        }
        None => u.push_str("\nCURRENT TASK: none\n"),
    }
    if !state.agent_notes.is_empty() {
        u.push_str("\nSYSTEM NOTES on the details you passed last turn (act on them now):\n");
        for n in &state.agent_notes {
            u.push_str(&format!("- {n}\n"));
        }
    }
    u.push_str(&format!("\nCALLER NOW: \"{transcript}\""));
    u
}

pub fn build_request(b: &Business, state: &CallState, transcript: &str) -> LlmRequest {
    let c = &b.config;
    let tasks: Vec<Value> = c.intents.iter().map(|i| json!(i.id)).chain([Value::Null]).collect();
    let slots: Vec<Value> = c.slots.keys().map(|k| json!(k)).collect();
    let actions: Vec<&str> = AgentAction::ALL.iter().map(|(n, _)| *n).collect();
    // `action` (a few tokens) then `say`: the runtime knows whether this turn reads back or
    // submits before any words arrive, and speaks the rest while it is still being generated.
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["action", "say", "task", "fields"],
        "properties": {
            "action": { "type": "string", "enum": actions },
            "say": { "type": "string" },
            "task": { "type": ["string", "null"], "enum": tasks },
            "fields": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["slot", "value"],
                    "properties": {
                        "slot": { "type": "string", "enum": slots },
                        "value": { "type": "string" }
                    }
                }
            }
        }
    });
    LlmRequest { system: system_prompt(b), user: turn_message(b, state, transcript), schema }
}

/// Read the model's JSON. Unknown tasks and slots are dropped here; values are validated by
/// the engine.
pub fn parse(b: &Business, reply: &Value) -> AgentTurn {
    let say = reply.get("say").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let action = AgentAction::parse(reply.get("action").and_then(Value::as_str).unwrap_or("none"));
    let task = reply.get("task").and_then(Value::as_str).filter(|t| b.intent(t).is_some()).map(str::to_string);
    let fields = reply
        .get("fields")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|f| {
            let slot = f.get("slot")?.as_str()?;
            let value = f.get("value")?.as_str()?.trim();
            (b.config.slots.contains_key(slot) && !value.is_empty()).then(|| (slot.to_string(), value.to_string()))
        })
        .collect();
    AgentTurn { say, action, task, fields }
}

/// Pulls finished sentences of the `say` value out of a JSON reply while it streams in, so
/// speech can start before the model has written the rest of its decision.
#[derive(Debug, Default)]
pub struct SayStream {
    raw: String,
    /// Characters of the decoded `say` already handed out.
    emitted: usize,
}

impl SayStream {
    /// Add a chunk of the reply; returns sentences completed by it.
    pub fn push(&mut self, delta: &str) -> Vec<String> {
        self.raw.push_str(delta);
        let Some((text, closed)) = decode_say(&self.raw) else { return Vec::new() };
        let chars: Vec<char> = text.chars().collect();
        let mut out = Vec::new();
        let mut start = self.emitted;
        let mut i = self.emitted;
        while i < chars.len() {
            let end_mark = matches!(chars[i], '.' | '?' | '!');
            // A mark ends a sentence once the next character shows it is not "3.5" or "...".
            let ended = end_mark
                && match chars.get(i + 1) {
                    Some(n) => n.is_whitespace(),
                    None => closed,
                };
            if ended {
                let sentence: String = chars[start..=i].iter().collect();
                if !sentence.trim().is_empty() {
                    out.push(sentence.trim().to_string());
                }
                start = i + 1;
            }
            i += 1;
        }
        self.emitted = start;
        out
    }

    /// The action, once the reply has got that far (it comes before `say`).
    pub fn action(&self) -> Option<AgentAction> {
        let key = self.raw.find("\"action\"")?;
        let after = &self.raw[key + 8..];
        let open = after.find('"')?;
        let value = &after[open + 1..];
        let close = value.find('"')?;
        Some(AgentAction::parse(&value[..close]))
    }

    /// Whatever is left of `say` once the reply is complete (a last sentence with no mark).
    pub fn rest(&mut self) -> Option<String> {
        let (text, _) = decode_say(&self.raw)?;
        let rest: String = text.chars().skip(self.emitted).collect();
        self.emitted = text.chars().count();
        Some(rest.trim().to_string()).filter(|r| !r.is_empty())
    }
}

/// The `say` string decoded so far, and whether its closing quote has arrived.
fn decode_say(raw: &str) -> Option<(String, bool)> {
    let key = raw.find("\"say\"")?;
    let after = &raw[key + 5..];
    let colon = after.find(':')?;
    let after = &after[colon + 1..];
    let quote = after.find('"')?;
    if !after[..quote].trim().is_empty() {
        return None;
    }
    let mut out = String::new();
    let mut chars = after[quote + 1..].chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => return Some((out, true)),
            '\\' => match chars.next() {
                Some('n') | Some('t') | Some('r') => out.push(' '),
                Some('u') => {
                    let hex: String = chars.by_ref().take(4).collect();
                    if hex.len() < 4 {
                        return Some((out, false));
                    }
                    if let Some(c) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        out.push(c);
                    }
                }
                Some(other) => out.push(other),
                None => return Some((out, false)),
            },
            c => out.push(c),
        }
    }
    Some((out, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn say_streams_sentence_by_sentence() {
        let mut s = SayStream::default();
        let mut got = Vec::new();
        for chunk in ["{\"sa", "y\": \"סגור", ". לאן נוס", "עים?\", \"action\": \"none\"", ", \"task\": null}"]
        {
            got.extend(s.push(chunk));
        }
        assert_eq!(got, vec!["סגור.", "לאן נוסעים?"]);
        assert_eq!(s.rest(), None);
    }

    #[test]
    fn the_action_is_known_before_the_words() {
        let mut s = SayStream::default();
        assert_eq!(s.action(), None);
        s.push("{\"action\": \"read_");
        assert_eq!(s.action(), None, "not complete yet");
        s.push("back\", \"say\": \"סג");
        assert_eq!(s.action(), Some(AgentAction::ReadBack));
    }

    #[test]
    fn a_sentence_without_a_mark_comes_out_at_the_end() {
        let mut s = SayStream::default();
        assert!(s.push("{\"say\":\"רגע אני בודק").is_empty());
        assert!(s.push("\",\"action\":\"none\"}").is_empty());
        assert_eq!(s.rest().as_deref(), Some("רגע אני בודק"));
    }

    #[test]
    fn numbers_and_escapes_do_not_split_or_break_the_text() {
        let mut s = SayStream::default();
        let got = s.push(r#"{"say": "בעוד 3.5 דקות, \"בדרך\". יאללה!", "action": "none"}"#);
        assert_eq!(got, vec!["בעוד 3.5 דקות, \"בדרך\".", "יאללה!"]);
    }
}

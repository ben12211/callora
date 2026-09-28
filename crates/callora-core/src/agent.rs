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

use crate::address_form::AddressForm;
use crate::business::Business;
use crate::config::SlotKind;
use crate::llm::LlmRequest;
use crate::state::{CallState, Speaker, Step};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentAction {
    /// Just talk.
    #[default]
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
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AgentTurn {
    /// A pre-recorded phrase (a response id from the business's `agent.phrases`), said
    /// before `say`. It plays the moment its id arrives: no text to generate, no TTS.
    pub phrase: Option<String>,
    pub say: String,
    pub action: AgentAction,
    /// Intent id the caller is on now, if any.
    pub task: Option<String>,
    /// (slot, value in the caller's words) given in this utterance.
    pub fields: Vec<(String, String)>,
}

/// The fixed wording of the business's instant phrases.
pub fn phrases(b: &Business) -> Vec<String> {
    phrase_ids(b).iter().filter_map(|id| b.config.responses.get(*id)).flat_map(|r| r.variants.iter().cloned()).collect()
}

/// Whether `sentence` of the agent's `say` is still said after its recorded `phrase`: never
/// the phrase's words again, and never a second question after a phrase that asks one. Live
/// calls asked every question twice: the model put the question in the phrase ("מאיפה
/// לאסוף?") and again, reworded, in `say` ("מאיזו עיר לאסוף?").
pub fn say_after_phrase(b: &Business, phrase: &str, sentence: &str) -> bool {
    let Some(r) = b.response(phrase) else { return true };
    let said = crate::text::normalize(sentence);
    if said.is_empty() || r.variants.iter().any(|v| crate::text::normalize(v) == said) {
        return false;
    }
    let phrase_asks = r.variants.first().is_some_and(|v| v.trim_end().ends_with('?'));
    !(phrase_asks && sentence.contains('?'))
}

/// The ids of the business's instant phrases: responses with nothing to fill in, so every
/// variant is a recorded clip.
pub fn phrase_ids(b: &Business) -> Vec<&str> {
    let Some(agent) = &b.config.agent else { return Vec::new() };
    agent
        .phrases
        .iter()
        .filter(|id| b.config.responses.get(*id).is_some_and(|r| r.variants.iter().all(|v| !v.contains('{'))))
        .map(String::as_str)
        .collect()
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
/// not change during a call, so providers can cache it. Nothing in it belongs to one kind of
/// business: the examples come from the business's `agent.prompt`, its rules from
/// `agent.rules`.
pub fn system_prompt(b: &Business) -> String {
    let c = &b.config;
    let agent = c.agent.as_ref();
    let words = agent.map(|a| &a.prompt);
    let eg = |example: Option<&String>| example.map(|e| format!(" ({e})")).unwrap_or_default();
    let role = words.and_then(|w| w.role.as_deref()).unwrap_or("receptionist");
    let mut s = String::new();
    if let Some(a) = agent {
        s.push_str(&a.persona);
        s.push_str("\n\n");
    }
    s.push_str(&format!(
        "You are the phone agent of \"{}\" (language {}). You run the conversation like an experienced human \
         {role}: listen, understand what the caller means, and move them to what they need in as few turns as \
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

    let instant = phrase_ids(b);
    s.push_str("\nREPLY with JSON, every turn:\n");
    s.push_str(
        "- action: \"none\"; \"read_back\" when every required detail of the task is known and the caller has \
         nothing to add: the system then reads the details back and asks to confirm, so add no question of your \
         own (at most a short acknowledgement); \"submit\" only when the caller just confirmed that read-back: \
         the system then sends it and tells the caller the result, so at most a short acknowledgement; \
         \"transfer\" for a human; \"end_call\" only when the caller clearly says goodbye or that they need \
         nothing more.\n",
    );
    // Addresses only for a business that takes places.
    let places = c.slots.values().any(|slot| slot.kind == SlotKind::Place);
    s.push_str(&format!(
        "- fields: details of the current task that the caller has given and that CURRENT TASK does not show \
         yet (from this utterance or an earlier one), copied in their words, without a leading preposition. A \
         detail you mention or confirm must be in CURRENT TASK or in your fields; otherwise the system does not \
         have it. Never invent or complete a value. A word after a preposition is a detail only if it names \
         one{}.",
        eg(words.and_then(|w| w.not_a_place.as_ref())),
    ));
    if places {
        s.push_str(&format!(
            " A place is street, number and city when the caller gave them{}, with the city from earlier in the \
             call if they said it then; the system checks places against the official list of localities and \
             streets.",
            eg(words.and_then(|w| w.place.as_ref())),
        ));
    }
    s.push('\n');
    if !instant.is_empty() {
        s.push_str(
            "- phrase: the id of an INSTANT PHRASE (below) that says what you mean, or null. A phrase starts \
             playing the moment you write its id, while any other words take the voice about a second to \
             produce: whenever one fits, use it.\n",
        );
    }
    s.push_str(&format!(
        "- say: what you say {}: at most ONE short, natural sentence in the caller's language, like a real \
         {role}. Never repeat the greeting, never list options unless the caller is lost, never say you did not \
         understand and then ask something else in the same turn. Unless the caller is saying goodbye, the turn \
         moves the call on: after taking a detail it asks the next question{}, never just an acknowledgement.\n",
        if instant.is_empty() {
            "now"
        } else {
            "instead of a phrase. With a phrase, say is \"\" (never the phrase again in other words, never a \
             second question)"
        },
        eg(words.and_then(|w| w.next_question.as_ref())),
    ));
    s.push_str("- task: the task the caller is on now, or null.\n");

    s.push_str(&format!(
        "\nRULES:\n\
         - Speech recognition makes mistakes. Act on what you did understand{}; if nothing makes sense, ask again. \
         Never guess a value{}, never end the call because of it.\n\
         - Greetings and small talk get a short friendly answer, then offer help.\n\
         - Prices, arrival times and availability come only from the system. Never promise them yourself.\n\
         - Ask only for what is still missing, the most important first; one short question may ask for two \
         missing details that go together.\n\
         - If the caller corrects a detail while details are being confirmed, take the correction and choose \
         read_back again.\n",
        eg(words.and_then(|w| w.garbled.as_ref())),
        if places { " (a number alone is a house number, not a street)" } else { "" },
    ));
    let optional = optional_details(b);
    if !optional.is_empty() {
        s.push_str(&format!(
            "- Never ask about optional details ({}) unless the caller brings them up.\n",
            optional.join(", ")
        ));
    }
    if let Some(a) = agent {
        for r in &a.rules {
            s.push_str(&format!("- {r}\n"));
        }
    }
    if !instant.is_empty() {
        s.push_str(
            "\nINSTANT PHRASES (id: its recorded wording, one variant is played). Use the id in phrase; never \
             write a phrase's words in say with something added (not even a city):\n",
        );
        for id in &instant {
            let variants = c.responses.get(*id).map(|r| r.variants.clone()).unwrap_or_default();
            let quoted: Vec<String> = variants.iter().map(|v| format!("\"{v}\"")).collect();
            s.push_str(&format!("- {id}: {}\n", quoted.join(" / ")));
        }
    }
    s
}

/// Details of any task that are optional and never asked for: the agent must not ask about
/// them either (a caller who wants one brings it up).
fn optional_details(b: &Business) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in b.config.pipelines.values() {
        for ps in &p.slots {
            if !ps.required && ps.ask.is_none() && ps.default.is_none() && !out.contains(&ps.slot) {
                out.push(ps.slot.clone());
            }
        }
    }
    out
}

/// A response the agent can name as a phrase: the id, with its wording for the model.
fn as_phrase(b: &Business, id: &str) -> Option<String> {
    if !phrase_ids(b).contains(&id) {
        return None;
    }
    let first = b.response(id)?.variants.first()?.clone();
    Some(format!("phrase {id} (\"{first}\")"))
}

/// The per-turn message: the conversation so far, where the task stands, what was just said.
pub fn turn_message(b: &Business, state: &CallState, transcript: &str) -> String {
    let words = b.config.agent.as_ref().map(|a| &a.prompt);
    let mut u = String::from("CONVERSATION:\n");
    for t in &state.history {
        let who = if t.speaker == Speaker::Agent { "Agent" } else { "Caller" };
        u.push_str(&format!("{who}: {}\n", t.text));
    }
    if let Some(name) = state.customer.as_ref().and_then(|c| c.name.as_deref()) {
        u.push_str(&format!("\nThe caller is a known customer: {name}.\n"));
    }
    if let Some(forms) = words.and_then(|w| w.address_forms.as_ref()) {
        let (form, how) = match state.address_form {
            AddressForm::Unknown => ("unknown", &forms.neutral),
            AddressForm::Masculine => ("masculine (the caller speaks of himself in masculine)", &forms.masculine),
            AddressForm::Feminine => ("feminine (the caller speaks of herself in feminine)", &forms.feminine),
        };
        u.push_str(&format!("\nADDRESS FORM: {form}. {how}\n"));
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
                            "- {}: city {}, street MISSING ({})\n",
                            ps.slot,
                            state.place_cities[&ps.slot],
                            if b.config.slots.get(&ps.slot).is_some_and(|c| c.precise) {
                                "required"
                            } else {
                                "ask once; the city is enough if the caller does not know"
                            }
                        )),
                        None if ps.default.is_some() => {
                            let d = ps.default.as_ref().map(|d| d.as_str().map_or(d.to_string(), str::to_string));
                            u.push_str(&format!("- {}: {} (default; do not ask)\n", ps.slot, d.unwrap_or_default()))
                        }
                        None if ps.required => u.push_str(&format!("- {}: MISSING (required)\n", ps.slot)),
                        None => u.push_str(&format!("- {}: not given (optional)\n", ps.slot)),
                    }
                }
            }
            // The street a place still needs: the model skipped the street after a destination
            // city in several calls, asked for the passengers and then talked past the caller's
            // street. Another missing detail may share the question.
            if let Some(p) = b.pipeline(&run.pipeline) {
                let pending = p.slots.iter().find(|ps| {
                    !run.slots.contains_key(&ps.slot) && ps.default.is_none() && (ps.required || ps.ask.is_some())
                });
                let street_ask = |ps: &crate::config::PipelineSlot| {
                    let id = format!("{}_street", ps.ask.as_ref()?);
                    as_phrase(b, &id).or_else(|| b.response(&id)?.variants.first().map(|v| format!("\"{v}\"")))
                };
                let place_with_street = |ps: &crate::config::PipelineSlot| {
                    b.config.slots.get(&ps.slot).is_some_and(|c| c.precise || c.street_once) && street_ask(ps).is_some()
                };
                if let Some(ps) = pending {
                    if state.place_cities.contains_key(&ps.slot) {
                        u.push_str(&format!(
                            "NOW: the caller is giving the street in {} for {}; take it (with the city) and go on.\n",
                            state.place_cities[&ps.slot], ps.slot
                        ));
                    } else if place_with_street(ps) {
                        u.push_str(&format!(
                            "NOW: the caller is giving the {} city. When they say only a city, your next question \
                             asks for its street ({}); it may also ask for one other missing detail.\n",
                            ps.slot,
                            street_ask(ps).unwrap_or_default()
                        ));
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
    let streak = state.same_question_streak();
    if streak >= 3 {
        let example =
            words.and_then(|w| w.stuck.as_ref()).map(|e| format!(", with an example ({e})")).unwrap_or_default();
        u.push_str(&format!(
            "\nSTUCK: you asked the same question {streak} times in a row and the caller keeps answering something \
             else. Do not ask it the same way again: say simply what you need and why{example}, or take what they \
             said if it answers something else. If it is optional, skip it.\n"
        ));
    }
    if !state.agent_notes.is_empty() {
        u.push_str("\nSYSTEM NOTES on the details you passed last turn (act on them now):\n");
        for n in &state.agent_notes {
            u.push_str(&format!("- {n}\n"));
        }
    }
    u.push_str(&format!("\nCALLER NOW: \"{transcript}\""));
    if let Some(second) = state.second_hearing.as_deref().filter(|s| !s.is_empty() && *s != transcript) {
        u.push_str(&format!(
            "\nSECOND HEARING of the same words (a slower, more accurate recognizer, hinted with the places \
             expected): \"{second}\". Where the two differ, go by the one that makes sense (usually this one)."
        ));
    }
    u
}

pub fn build_request(b: &Business, state: &CallState, transcript: &str) -> LlmRequest {
    let c = &b.config;
    let tasks: Vec<Value> = c.intents.iter().map(|i| json!(i.id)).chain([Value::Null]).collect();
    let slots: Vec<Value> = c.slots.keys().map(|k| json!(k)).collect();
    let actions: Vec<&str> = AgentAction::ALL.iter().map(|(n, _)| *n).collect();
    // `action` (a few tokens), `fields`, then `phrase` and `say`: the runtime knows whether
    // this turn reads back or submits, and whether its values will be accepted, before any
    // words arrive; a phrase plays as soon as its id is complete, and `say` is spoken while
    // it is still being generated.
    let mut properties = serde_json::Map::new();
    properties.insert("action".into(), json!({ "type": "string", "enum": actions }));
    properties.insert(
        "fields".into(),
        json!({
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
        }),
    );
    let phrases: Vec<Value> = phrase_ids(b).into_iter().map(|id| json!(id)).collect();
    if !phrases.is_empty() {
        let options: Vec<Value> = phrases.into_iter().chain([Value::Null]).collect();
        properties.insert("phrase".into(), json!({ "type": ["string", "null"], "enum": options }));
    }
    properties.insert("say".into(), json!({ "type": "string" }));
    properties.insert("task".into(), json!({ "type": ["string", "null"], "enum": tasks }));
    let required: Vec<String> = properties.keys().cloned().collect();
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "required": required,
        "properties": properties,
    });
    LlmRequest { system: system_prompt(b), user: turn_message(b, state, transcript), schema }
}

/// Read the model's JSON. Unknown tasks and slots are dropped here; values are validated by
/// the engine.
pub fn parse(b: &Business, reply: &Value) -> AgentTurn {
    let say = reply.get("say").and_then(Value::as_str).unwrap_or("").trim().to_string();
    let action = AgentAction::parse(reply.get("action").and_then(Value::as_str).unwrap_or("none"));
    let task = reply.get("task").and_then(Value::as_str).filter(|t| b.intent(t).is_some()).map(str::to_string);
    let phrase = reply.get("phrase").and_then(Value::as_str).filter(|p| phrase_ids(b).contains(p)).map(str::to_string);
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
    AgentTurn { phrase, say, action, task, fields }
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

    /// The fields, once the reply has got past them (to `phrase` or `say`): the runtime
    /// checks them before a word is spoken. "47" passengers was rejected after the agent had
    /// already asked the next question, and the call went out of step.
    pub fn fields(&self) -> Option<Vec<(String, String)>> {
        let key = self.raw.find("\"fields\"")?;
        let after = &self.raw[key..];
        let end = [after.find("\"phrase\""), after.find("\"say\"")].into_iter().flatten().min()?;
        let head = &after[..end];
        let open = head.find('[')?;
        let close = head.rfind(']')?;
        let items: Vec<Value> = serde_json::from_str(&head[open..=close]).ok()?;
        Some(
            items
                .iter()
                .filter_map(|f| Some((f.get("slot")?.as_str()?.to_string(), f.get("value")?.as_str()?.to_string())))
                .collect(),
        )
    }

    /// The recorded phrase to play, once its id is complete (`None` for null or not yet).
    pub fn phrase(&self) -> Option<String> {
        let key = self.raw.find("\"phrase\"")?;
        let after = &self.raw[key + 8..];
        let value = after[after.find(':')? + 1..].trim_start();
        let id = value.strip_prefix('"')?;
        let close = id.find('"')?;
        Some(id[..close].to_string()).filter(|p| !p.is_empty())
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
    fn the_phrase_is_known_as_soon_as_its_id_is_complete() {
        let mut s = SayStream::default();
        s.push(
            "{\"action\": \"none\", \"fields\": [{\"slot\": \"passengers\", \"value\": \"3\"}], \"phrase\": \"ask_na",
        );
        assert_eq!(s.fields(), Some(vec![("passengers".to_string(), "3".to_string())]), "fields end at the phrase");
        assert_eq!(s.phrase(), None, "not complete yet");
        s.push("me\", \"say\": \"\", \"task\": \"book_ride\"}");
        assert_eq!(s.phrase().as_deref(), Some("ask_name"));
        assert_eq!(s.rest(), None);

        let mut none = SayStream::default();
        none.push("{\"action\": \"none\", \"fields\": [], \"phrase\": null, \"say\": \"שלום.\"}");
        assert_eq!(none.phrase(), None);
    }

    #[test]
    fn nothing_after_a_question_phrase_asks_again() {
        let b = Business::from_json(include_str!("../../../businesses/taxi.json"), "taxi.json", &|_| None).unwrap();
        assert!(!say_after_phrase(&b, "ask_pickup", "מאיזו עיר לאסוף?"), "the same question reworded");
        assert!(!say_after_phrase(&b, "ask_pickup", "מאיפה אוספים?"), "another variant of the phrase");
        assert!(say_after_phrase(&b, "ack", "מאיזו עיר לאסוף?"), "an acknowledgement, then the question");
        assert!(say_after_phrase(&b, "small_talk_short", "צריך מונית?"), "no question in the phrase");
        assert!(!say_after_phrase(&b, "ack", "סגור."), "the acknowledgement twice");
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

//! The conversation engine: understanding in, directives out.
//!
//! The engine owns the [`CallState`] and decides what happens next. It never performs I/O:
//! speaking, running business actions, transferring and hanging up are [`Directive`]s the
//! runtime executes in order. That keeps every conversational decision testable, and it
//! keeps slow things (TTS, actions, the network) out of the decision path.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::agent::{AgentAction, AgentTurn};
use crate::business::Business;
use crate::config::{AfterPipeline, MetaIntent, PipelineConfig, RuleEffect};
use crate::customer::Customer;
use crate::gazetteer::{Gazetteer, Lookup};
use crate::render::{RenderContext, Renderer, SeededChooser, SpeechPlan};
use crate::state::{CallState, CompletedRun, Phase, PipelineRun, Prompt, SlotState, Speaker, Step, Turn};
use crate::understanding::{default_value, parse_slot_value, Context, Understanding};
use crate::values::{Provenance, SlotFill, SlotValue};

/// Something the runtime must do, in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "directive", rename_all = "snake_case")]
pub enum Directive {
    /// Play this plan (after anything already queued).
    Speak { plan: SpeechPlan, filler: bool },
    /// Run a business action; report back with [`Engine::on_action_result`].
    RunAction { run_id: u64, action: String, input: serde_json::Value },
    /// Transfer to a human once queued speech has played.
    Handoff { summary: HandoffSummary },
    /// Hang up once queued speech has played.
    Hangup,
}

/// Why a business action returned no result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionFailure {
    pub error: String,
    /// The request may have reached the business before it failed (a timeout after it was
    /// sent): the task may well have gone through, so it is neither reported as failed nor
    /// sent again blindly.
    #[serde(default)]
    pub outcome_unknown: bool,
}

impl ActionFailure {
    pub fn failed(error: impl Into<String>) -> Self {
        Self { error: error.into(), outcome_unknown: false }
    }

    pub fn unknown(error: impl Into<String>) -> Self {
        Self { error: error.into(), outcome_unknown: true }
    }
}

impl From<String> for ActionFailure {
    fn from(error: String) -> Self {
        Self::failed(error)
    }
}

impl From<&str> for ActionFailure {
    fn from(error: &str) -> Self {
        Self::failed(error)
    }
}

impl std::fmt::Display for ActionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.error)?;
        if self.outcome_unknown {
            f.write_str(" (outcome unknown)")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandoffSummary {
    pub reason: String,
    pub business_id: String,
    pub intent: Option<String>,
    pub pipeline: Option<String>,
    /// (slot description, spoken value)
    pub slots: Vec<(String, String)>,
    pub customer_name: Option<String>,
    pub recent: Vec<Turn>,
    /// Spoken to the human agent before the caller is connected.
    pub text: String,
}

/// Directive builder that keeps speech ordered relative to other directives and tracks
/// what should be remembered as "the last thing said" for repeats.
#[derive(Default)]
struct Out {
    directives: Vec<Directive>,
    pending: Option<SpeechPlan>,
    recorded: Option<SpeechPlan>,
}

impl Out {
    fn speak(&mut self, plan: SpeechPlan, record: bool) {
        if record {
            self.recorded = Some(match self.recorded.take() {
                Some(r) => r.then(plan.clone()),
                None => plan.clone(),
            });
        }
        self.pending = Some(match self.pending.take() {
            Some(p) => p.then(plan),
            None => plan,
        });
    }

    fn flush(&mut self, filler: bool) {
        if let Some(plan) = self.pending.take() {
            if !plan.is_empty() {
                self.directives.push(Directive::Speak { plan, filler });
            }
        }
    }

    fn push(&mut self, d: Directive) {
        self.flush(false);
        self.directives.push(d);
    }

    /// Something is already said in this turn.
    fn has_speech(&self) -> bool {
        self.pending.as_ref().is_some_and(|p| !p.is_empty())
            || self.directives.iter().any(|d| matches!(d, Directive::Speak { plan, .. } if !plan.is_empty()))
    }
}

fn prompt(response: &str, values: &[(&str, String)]) -> Prompt {
    Prompt { response: response.to_string(), values: values.iter().map(|(k, v)| (k.to_string(), v.clone())).collect() }
}

/// Required details of the current task asked for and still not given, in the order asked.
pub fn open_questions(b: &Business, state: &CallState) -> Vec<String> {
    let Some(run) = &state.run else { return Vec::new() };
    let Some(pipeline) = b.pipeline(&run.pipeline) else { return Vec::new() };
    state
        .open_questions
        .iter()
        .filter(|s| pipeline.slots.iter().any(|ps| ps.slot == **s && ps.required && ps.default.is_none()))
        .filter(|s| !run.slots.contains_key(*s))
        .cloned()
        .collect()
}

pub struct Engine {
    business: Arc<Business>,
    pub state: CallState,
    chooser: SeededChooser,
    /// "Anything else?" was the last question.
    offered_more: bool,
    /// Israel's localities and streets, for checking the places the agent passes on.
    gazetteer: Option<Arc<Gazetteer>>,
    /// Whether a person can take the call, from the owner's settings (`None`: the business
    /// file's handoff number decides).
    desk: Option<bool>,
}

impl Engine {
    pub fn new(business: Arc<Business>, seed: u64) -> Self {
        let state = CallState::new(&business.config.id);
        Self { business, state, chooser: SeededChooser(seed | 1), offered_more: false, gazetteer: None, desk: None }
    }

    /// The city whose street the question just asked for ("מאיזו עיר?" "בני ברק" ... "איזה
    /// רחוב?"): the runtime biases recognition with its streets. Only while the street is
    /// asked: kept on, a live call heard the caller's name as a street of that city.
    pub fn street_focus(&self) -> Option<String> {
        let run = self.state.run.as_ref()?;
        // Also while a street that was not found is being cleared up ("יש כתובת של המקום?"
        // asks nothing by name): the answer is still a street of that city.
        // Only while nothing else is asked: after "כמה נוסעים?" the answer is a number.
        let clearing = |slot: &String| {
            self.state.last_asks.is_empty()
                && (self.state.place_rejections.contains_key(slot) || self.state.doubted_streets.contains(slot))
                && !run.slots.contains_key(slot)
        };
        self.pipeline_of(run)
            .slots
            .iter()
            .filter(|ps| self.state.last_asks.contains(&ps.slot) || clearing(&ps.slot))
            .find_map(|ps| self.state.place_cities.get(&ps.slot).cloned())
    }

    /// The call is about to hear a place's city ("מאיזו עיר לאסוף?"): the task's next
    /// missing detail is a place that needs a street, and no city for it yet.
    pub fn awaiting_city(&self) -> bool {
        let Some(run) = self.state.run.as_ref() else { return false };
        let pending =
            self.pipeline_of(run).slots.iter().find(|ps| {
                !run.slots.contains_key(&ps.slot) && ps.default.is_none() && (ps.required || ps.ask.is_some())
            });
        pending.is_some_and(|ps| {
            !self.state.place_cities.contains_key(&ps.slot)
                && self.business.config.slots.get(&ps.slot).is_some_and(|c| c.precise || c.street_once)
        })
    }

    /// Whether any of these values would be rejected (47 passengers when the most is 20, a
    /// street its city does not have) or would make a business rule take the turn over (17
    /// passengers go to a person): the runtime then holds the agent's words back, since
    /// they move on without it. The same checks as the turn itself, on a copy of the call: a
    /// live call's "מהשערה 18, אפרת" was refused only after "לאיזה רחוב נוסעים?" had played.
    pub fn rejects_any(&self, transcript: &str, fields: &[(String, String)]) -> bool {
        let mut probe = Engine {
            business: self.business.clone(),
            state: self.state.clone(),
            chooser: self.chooser.clone(),
            offered_more: self.offered_more,
            gazetteer: self.gazetteer.clone(),
            desk: self.desk,
        };
        probe.state.remember(Speaker::Caller, transcript);
        let fields = probe.by_preposition(transcript, fields);
        let fields = probe.with_patterns(transcript, &fields);
        let fields = probe.answer_in_place(transcript, &fields);
        let fields = probe.with_cues(transcript, &fields);
        let rejected = !probe.apply_agent_fields(&fields).1.is_empty();
        // Or a business rule takes the turn over (17 passengers: a person arranges it), or a
        // town said alone is neither place yet ("מבני ברק או לבני ברק?").
        rejected || probe.rule_takes_over() || self.lone_city_either_way(transcript, &fields).is_some()
    }

    /// A town said alone, with no מ or ל ("בני ברק."), while neither the pickup nor the
    /// destination is known, that the agent made one of them: it could be either. A live call's
    /// "בני ברק" to "מאיפה לאן?" became the pickup and was the destination.
    pub fn lone_city_either_way(&self, transcript: &str, fields: &[(String, String)]) -> Option<String> {
        let g = self.gazetteer.as_ref()?;
        let places_known = self
            .state
            .run
            .as_ref()
            .is_some_and(|r| r.slots.contains_key("pickup") || r.slots.contains_key("destination"));
        if places_known {
            return None;
        }
        let words: String =
            crate::text::normalize(transcript).chars().filter(|c| !matches!(c, '.' | ',' | '?' | '!')).collect();
        let said = self.business.fillers.strip(&words).trim().to_string();
        let crate::gazetteer::Lookup::Found(a) = g.resolve(&said) else { return None };
        // The town's own name and nothing else: a מ or ל before it says which place it is.
        if a.street.is_some() || crate::text::normalize(&a.city_said) != said {
            return None;
        }
        let made_a_place = fields.iter().any(|(slot, value)| {
            (slot == "pickup" || slot == "destination") && crate::text::normalize(value).contains(&said)
        });
        made_a_place.then(|| a.city_said.clone())
    }

    /// The detail a task with a fixed order (`strict_order`) asks for next: the first one
    /// missing, a required one until it is given, an optional one until it was asked once.
    pub fn expected_slot(&self) -> Option<String> {
        let run = self.state.run.as_ref()?;
        let pipeline = self.pipeline_of(run);
        if !pipeline.strict_order || !matches!(run.step, Step::Collecting { .. }) {
            return None;
        }
        pipeline
            .slots
            .iter()
            .find(|ps| {
                let known_name = ps.from_customer.as_deref() == Some("name") && self.customer_gives(ps).is_some();
                !run.slots.contains_key(&ps.slot)
                    && !known_name
                    && ps.default.is_none()
                    && (ps.required
                        || (ps.ask.is_some()
                            && !self.state.asked_slots.contains(&ps.slot)
                            && !self.state.asked_before_confirm.contains(&ps.slot)))
            })
            .map(|ps| ps.slot.clone())
    }

    /// The detail next in the order, when a question asking for `asks` would be out of it,
    /// with this reply's values taken (on a copy of the call). The runtime holds the question
    /// back and the engine asks the right one.
    pub fn out_of_order(&self, transcript: &str, fields: &[(String, String)], asks: &[String]) -> Option<String> {
        if asks.is_empty() {
            return None;
        }
        let mut probe = Engine {
            business: self.business.clone(),
            state: self.state.clone(),
            chooser: self.chooser.clone(),
            offered_more: self.offered_more,
            gazetteer: self.gazetteer.clone(),
            desk: self.desk,
        };
        probe.state.remember(Speaker::Caller, transcript);
        // No task yet (the call's first turn): the agent names it only after its words, so the
        // task is the one whose questions these are. "לאן צריך להגיע?" was asked before
        // "מאיפה?" on a first turn, and the call went wrong from there.
        if probe.state.run.is_none() {
            let task = self.business.config.intents.iter().find_map(|i| {
                let p = i.pipeline.as_ref()?;
                let pipeline = self.business.pipeline(p)?;
                (pipeline.strict_order && asks.iter().any(|a| pipeline.slots.iter().any(|s| &s.slot == a)))
                    .then(|| (p.clone(), i.id.clone()))
            });
            if let Some((pipeline, intent)) = task {
                probe.state.run = Some(probe.new_run(&pipeline, &intent));
                probe.fill_from_other_tasks();
            }
        }
        let fields = probe.by_preposition(transcript, fields);
        let fields = probe.with_patterns(transcript, &fields);
        let fields = probe.answer_in_place(transcript, &fields);
        let fields = probe.with_cues(transcript, &fields);
        probe.apply_agent_fields(&fields);
        probe.expected_slot().filter(|next| !asks.contains(next))
    }

    /// A place, before `slot` in the task's order, of which only part is known (its city, or a
    /// street waiting for its number): its question comes first.
    fn partial_place_before(&self, slot: &str) -> Option<String> {
        let run = self.state.run.as_ref()?;
        self.pipeline_of(run)
            .slots
            .iter()
            .take_while(|ps| ps.slot != slot)
            .filter(|ps| ps.required && !run.slots.contains_key(&ps.slot))
            .find(|ps| {
                self.state.place_cities.contains_key(&ps.slot)
                    || self.state.place_numbers.contains_key(&ps.slot)
                    || self.state.place_streets.contains_key(&ps.slot)
            })
            .map(|ps| ps.slot.clone())
    }

    /// Whether the agent's recorded question, with these values taken, would ask past a place
    /// given only in part ("מבני ברק לירושלים" answered by "כמה נוסעים?"; a live call asked for
    /// the street after the head count). The runtime then holds it back.
    pub fn phrase_skips_a_place(&self, transcript: &str, fields: &[(String, String)], phrase: &str) -> bool {
        let Some(asked) = self.slot_asked_by(phrase) else { return false };
        let mut probe = Engine {
            business: self.business.clone(),
            state: self.state.clone(),
            chooser: self.chooser.clone(),
            offered_more: self.offered_more,
            gazetteer: self.gazetteer.clone(),
            desk: self.desk,
        };
        probe.state.remember(Speaker::Caller, transcript);
        let fields = probe.answer_in_place(transcript, fields);
        probe.apply_agent_fields(&fields);
        probe.partial_place_before(&asked).is_some()
    }

    /// The required detail a reply would move on past: asked for earlier, not in this reply's
    /// values, while its question asks only for other details. Live calls asked for the
    /// passengers with the street still unknown, then went back to the street.
    pub fn moves_on(&self, fields: &[(String, String)], asks: &[String]) -> Option<String> {
        if asks.is_empty() {
            return None;
        }
        let run = self.state.run.as_ref()?;
        if !matches!(run.step, Step::Collecting { .. } | Step::ConfirmingSlot { .. }) {
            return None;
        }
        let open = open_questions(&self.business, &self.state);
        // Still on an open detail: its street after its city ("מאפרת" → "מאיזה רחוב?") goes on.
        if asks.iter().any(|a| open.contains(a)) {
            return None;
        }
        open.into_iter().find(|s| !fields.iter().any(|(f, v)| f == s && !v.trim().is_empty()))
    }

    /// The details the question just asked for: each required one stays open until it is
    /// given, and the caller's next answer is taken as the answer to it.
    fn note_asked(&mut self, slots: &[String]) {
        self.state.last_asks = slots.to_vec();
        self.state.asked_slots.extend(slots.iter().cloned());
        for slot in slots {
            if !self.state.open_questions.contains(slot) {
                self.state.open_questions.push(slot.clone());
            }
        }
        let open = open_questions(&self.business, &self.state);
        self.state.open_questions.retain(|s| open.contains(s));
    }

    /// Ask again for a detail the caller has not given: the street of a place whose city is
    /// known by its own question ("לאיזה רחוב צריך להגיע?", never a bare "which street?"
    /// with two places open), other details by their "to avoid a mistake" wording.
    fn ask_again(&mut self, out: &mut Out, slot: &str) {
        let again = format!("ask_again_{slot}");
        if !self.state.place_cities.contains_key(slot) && self.business.response(&again).is_some() {
            let ctx = self.render_ctx(None);
            self.say(out, &again, ctx, true);
            self.note_asked(&[slot.to_string()]);
        } else {
            self.ask(out, slot, false);
        }
    }

    pub fn set_caller_phone(&mut self, phone: Option<String>) {
        self.state.caller_phone = phone.filter(|p| !p.trim().is_empty());
    }

    /// Whether the dispatch desk has numbers to ring (the owner's settings).
    pub fn set_desk(&mut self, available: bool) {
        self.desk = Some(available);
    }

    fn has_desk(&self) -> bool {
        self.desk.unwrap_or(self.business.handoff_number.is_some())
    }

    pub fn set_gazetteer(&mut self, gazetteer: Option<Arc<Gazetteer>>) {
        self.gazetteer = gazetteer;
    }

    pub fn business(&self) -> &Arc<Business> {
        &self.business
    }

    /// What the understanding layer needs to know about the conversation right now.
    pub fn context(&self) -> Context<'_> {
        let run = self.state.run.as_ref();
        Context {
            active_pipeline: run.map(|r| r.pipeline.as_str()),
            awaiting_slot: run.and_then(PipelineRun::awaiting_slot),
            awaiting_confirmation: matches!(run.map(|r| &r.step), Some(Step::AwaitingConfirmation))
                || matches!(run.map(|r| &r.step), Some(Step::ConfirmingSlot { .. })),
            customer: self.state.customer.as_ref(),
        }
    }

    pub fn set_customer(&mut self, customer: Option<Customer>) {
        self.state.customer = customer;
    }

    /// The customer lookup to run at call start, if the business has one.
    pub fn customer_lookup(&self, caller: Option<&str>) -> Option<(String, serde_json::Value)> {
        let cfg = self.business.config.customer_lookup.as_ref()?;
        let caller = caller?;
        Some((cfg.action.clone(), json!({ "phone": caller })))
    }

    /// The greeting. Call once, as soon as the call connects.
    pub fn start(&mut self) -> Vec<Directive> {
        let mut out = Out::default();
        // A returning caller hears their last ride, a known one their name; whichever cannot be
        // said (no name to put in it) gives way to the next, down to the plain greeting.
        let lookup = self.business.config.customer_lookup.as_ref();
        let known = self.state.customer.as_ref().is_some_and(|c| c.name.is_some());
        let returning = self.state.customer.as_ref().is_some_and(|c| c.data.contains_key("last_to_city"));
        let candidates = [
            lookup.and_then(|l| l.returning_greeting.clone()).filter(|_| returning),
            lookup.and_then(|l| l.known_greeting.clone()).filter(|_| known),
            Some(self.business.config.greeting.clone()),
        ];
        let ctx = RenderContext { customer: self.state.customer.as_ref(), ..Default::default() }.into_owned();
        for greeting in candidates.into_iter().flatten() {
            if let Some(plan) = self.render_plan(&greeting, &ctx) {
                out.speak(plan, true);
                break;
            }
        }
        self.finish(out)
    }

    /// Utterances with one certain meaning skip the agent: "רגע", "מה?", "לא שמעתי", and a
    /// plain yes to a read-back (so a booking goes out the moment the caller confirms it).
    /// They go to [`Engine::on_utterance`] instead.
    pub fn fast_lane(&self, u: &Understanding, needs_llm: bool) -> bool {
        if needs_llm || u.intent.is_some() || !u.slots.is_empty() {
            return false;
        }
        match u.meta {
            Some(m) => matches!(
                m,
                MetaIntent::Wait
                    | MetaIntent::RepeatLast
                    | MetaIntent::DidNotUnderstand
                    | MetaIntent::SpeakSlower
                    | MetaIntent::SpeakLouder
            ),
            None => {
                u.affirm == Some(true)
                    && u.coverage >= 0.99
                    && self.state.run.as_ref().is_some_and(|r| r.step == Step::AwaitingConfirmation)
            }
        }
    }

    /// Say the last reply again, e.g. after line noise cut it off.
    pub fn replay_last(&mut self) -> Vec<Directive> {
        let mut out = Out::default();
        if self.state.phase == Phase::Active {
            if let Some(last) = self.state.last_plan.clone() {
                out.speak(last, true);
            }
        }
        self.finish(out)
    }

    /// A decision of the LLM agent for the caller's last utterance. `spoken` is what the
    /// runtime already played of it (its phrase and sentences of its `say`) while the
    /// decision streamed in.
    ///
    /// The agent chooses; the engine enforces: values go through the typed parsers, a task
    /// runs only after its read-back was confirmed, and the call ends only on a goodbye.
    pub fn on_agent_turn(&mut self, transcript: &str, turn: AgentTurn, spoken: &str) -> Vec<Directive> {
        let mut out = Out::default();
        if self.state.phase != Phase::Active {
            return Vec::new();
        }
        self.state.turns += 1;
        self.state.silence_reprompts = 0;
        self.state.fallback_level = 0;
        self.state.waiting = false;
        let offered_more = std::mem::take(&mut self.offered_more);
        self.state.remember(Speaker::Caller, transcript);
        // Notes were for the decision just made; new ones are for the next.
        self.state.agent_notes.clear();
        let mut turn = turn;
        if let Some(intent) = turn.phrase.as_deref().and_then(|p| self.refused_wrongly(transcript, p)) {
            tracing::info!(transcript, %intent, "a refusal of what the business does; its task instead");
            turn.phrase = None;
            turn.say.clear();
            turn.task = Some(intent);
        }
        // The same refusal when the caller's words were too garbled to show the task, but the
        // agent's own decision is one of the business's tasks, or has its details: a live call
        // heard "את זה אני לא יודע, אני פה רק בשביל מוניות" and then the price it asked for.
        let refusal = turn.phrase.as_deref().is_some_and(|p| self.business.response(p).is_some_and(|r| r.alone));
        let business_task =
            turn.task.as_deref().and_then(|t| self.business.intent(t)).is_some_and(|i| i.pipeline.is_some());
        if refusal && (business_task || !turn.fields.is_empty()) {
            tracing::info!(transcript, phrase = ?turn.phrase, task = ?turn.task, "a refusal with a business task; not said");
            turn.phrase = None;
            turn.say.clear();
        }
        // Words that decide the task (a price question) over the agent's choice: its words were
        // for the other task, the task's own next step is said instead.
        let mut decided = false;
        if let Some(intent) = self.decisive_intent(transcript).filter(|i| turn.task.as_deref() != Some(i.as_str())) {
            tracing::info!(transcript, %intent, agent = ?turn.task, "the caller's words decide the task");
            turn.phrase = None;
            turn.say.clear();
            turn.task = Some(intent);
            if turn.action == AgentAction::None || turn.action == AgentAction::ReadBack {
                turn.action = AgentAction::None;
            }
            decided = true;
        }
        // Asked the same thing five times over and still stuck: a person takes the call (or,
        // with no desk, it ends politely) rather than a sixth round of the same question.
        if self.state.same_question_streak() >= 5 {
            tracing::warn!(transcript, "the same question five times; handing off");
            return self.force_handoff("stuck_on_a_question");
        }

        // The task.
        if let Some(intent) = turn.task.as_deref().and_then(|t| self.business.intent(t)).cloned() {
            if intent.handoff {
                self.agent_say(&mut out, turn.phrase.as_deref(), &turn.say, spoken);
                self.handoff(&mut out, &format!("intent:{}", intent.id));
                return self.finish(out);
            }
            if let Some(p) = &intent.pipeline {
                if self.state.run.as_ref().map(|r| &r.pipeline) != Some(p) {
                    if let Some(run) = self.state.run.take() {
                        if !run.slots.is_empty() && !matches!(run.step, Step::Executing { .. }) {
                            self.state.suspended.push(run);
                        }
                    }
                    self.state.run = Some(self.new_run(p, &intent.id));
                    self.fill_from_other_tasks();
                }
            }
        }
        if let Some(city) = self.lone_city_either_way(transcript, &turn.fields) {
            if self.business.response("which_way").is_some() {
                tracing::info!(transcript, %city, "a town alone, neither place yet: asked which");
                self.agent_say(&mut out, None, "", spoken);
                let mut ctx = self.render_ctx(None);
                ctx.extra.insert("city".into(), city);
                self.say(&mut out, "which_way", ctx, true);
                self.note_asked(&["pickup".to_string()]);
                return self.finish(out);
            }
        }
        let was_confirming = self.state.run.as_ref().is_some_and(|r| r.step == Step::AwaitingConfirmation);
        self.misplaced_city(transcript);
        let fields = self.street_answer(transcript, &turn);
        let fields = self.by_preposition(transcript, &fields);
        let fields = self.with_patterns(transcript, &fields);
        let fields = self.answer_in_place(transcript, &fields);
        let fields = self.with_cues(transcript, &fields);
        let (changed, rejected) = self.apply_agent_fields(&fields);
        // The business's rules, as in a turn without the agent: a live call booked 17
        // passengers in one taxi, though more than 8 go to a person.
        if self.apply_rules(&mut out) {
            tracing::info!(transcript, "a business rule took the turn over");
            self.agent_say(&mut out, None, "", spoken);
            return self.finish(out);
        }
        // A detail changed after the read-back ("לא 40, 45"): read it back again, so the next
        // "יאללה" sends the corrected task instead of meeting another read-back.
        let action = if turn.action == AgentAction::None && changed && was_confirming {
            AgentAction::ReadBack
        } else {
            turn.action
        };
        // "רגע, מעביר למוקדן" with no transfer: words that promise what does not happen.
        if action != AgentAction::Transfer && self.business.announces_transfer(&turn.say) {
            tracing::info!(say = %turn.say, "a transfer announced without one; not said");
            turn.say.clear();
        }
        // Before the first read-back, the optional question the business always wants asked
        // ("יש משהו שהנהג צריך לדעת?"), when the agent skipped it.
        if matches!(action, AgentAction::ReadBack | AgentAction::Submit) && !was_confirming && rejected.is_empty() {
            if let Some(slot) = self.unasked_before_confirm() {
                self.agent_say(&mut out, None, "", spoken);
                self.state.asked_before_confirm.insert(slot.clone());
                if !spoken.trim_end().ends_with('?') {
                    self.ask(&mut out, &slot, false);
                }
                return self.finish(out);
            }
        }
        // A detail just rejected: ask for it, never read back or send what was there before,
        // nor the agent's next question (held back by the runtime, so nothing was said).
        if matches!(action, AgentAction::ReadBack | AgentAction::Submit)
            || (action == AgentAction::None && spoken.is_empty() && !rejected.is_empty())
        {
            if let Some(slot) = rejected.first().cloned() {
                // The agent's "סגור." was for a read-back that is not coming.
                self.agent_say(&mut out, None, "", spoken);
                // A place missed three times: a person, with what was collected.
                if self.business.handoff_number.is_some()
                    && self.state.place_rejections.get(&slot).copied().unwrap_or(0) >= 3
                {
                    tracing::info!(%slot, "a place was not understood three times; handing off");
                    return self.force_handoff("place_not_understood");
                }
                // What was not found, and the question about it.
                if let Some(prompts) = self.state.doubt_confirm.remove(&slot).filter(|_| spoken.is_empty()) {
                    for p in prompts {
                        let mut ctx = self.render_ctx(None);
                        ctx.extra.extend(p.values);
                        self.say(&mut out, &p.response, ctx, true);
                    }
                    self.note_asked(&[slot]);
                    return self.finish(out);
                }
                if !spoken.trim_end().ends_with('?') {
                    // "רק כדי שלא תהיה טעות, כמה נוסעים?"; a number past the slot's maximum
                    // ("מאה", "אלף") is told so first: a live call heard the question again,
                    // twice, as if the answer had not been heard.
                    let too_many = format!("too_many_{slot}");
                    let again = if self.business.response(&too_many).is_some() && self.above_max(&slot, &fields) {
                        too_many
                    } else {
                        format!("ask_again_{slot}")
                    };
                    if self.business.response(&again).is_some() {
                        let ctx = self.render_ctx(None);
                        self.say(&mut out, &again, ctx, true);
                    } else {
                        self.ask(&mut out, &slot, false);
                    }
                }
                return self.finish(out);
            }
        }

        // The model took the answer and asked the same question again (a live call: "בן זכאי,
        // 32" stored as the destination, then "לאיזה רחוב?"). The question was answered: ask
        // the next one instead.
        // Or took part of it: "מאלעד לבני ברק" noted the city, and the model's "מאיזו עיר לאסוף?"
        // asked for it again (a live call of 2026-09-30).
        let answered = spoken.is_empty() && action == AgentAction::None && {
            let asked = turn.phrase.as_deref().and_then(|p| self.slot_asked_by(p));
            asked.is_some_and(|slot| {
                turn.fields.iter().any(|(s, _)| *s == slot)
                    && (self.state.run.as_ref().is_some_and(|r| r.slots.contains_key(&slot))
                        || self.state.place_cities.contains_key(&slot)
                        || self.state.place_numbers.contains_key(&slot)
                        || self.state.place_streets.contains_key(&slot))
            })
        };
        if answered {
            tracing::info!(transcript, phrase = ?turn.phrase, "the agent asked again for what the caller just gave; asking the next question");
            self.next_question(&mut out);
            return self.finish(out);
        }

        // A task with a fixed order: a question about any other detail than the next is replaced
        // by the next one ("כמה נוסעים?" with the destination still missing).
        if spoken.is_empty() && action == AgentAction::None {
            let mut asks = turn.asks.clone();
            if let Some(slot) = turn.phrase.as_deref().and_then(|p| self.slot_asked_by(p)) {
                asks.push(slot);
            }
            if !asks.is_empty() {
                if let Some(next) = self.expected_slot().filter(|next| !asks.contains(next)) {
                    tracing::info!(transcript, ?asks, %next, "a question out of the task's order; asking the next one");
                    self.next_question(&mut out);
                    return self.finish(out);
                }
            }
        }
        // Its recorded question asks past a place given only in part: that place's question.
        if spoken.is_empty() && action == AgentAction::None {
            let asked = turn.phrase.as_deref().and_then(|p| self.slot_asked_by(p));
            if let Some(place) = asked.and_then(|slot| self.partial_place_before(&slot)) {
                tracing::info!(transcript, phrase = ?turn.phrase, %place, "the question skips a place given in part; asking for it first");
                self.next_question(&mut out);
                return self.finish(out);
            }
        }

        // What this turn's question asks for, with its recorded phrase's detail.
        let mut asks = turn.asks.clone();
        if let Some(slot) = turn.phrase.as_deref().and_then(|p| self.slot_asked_by(p)) {
            if !asks.contains(&slot) {
                asks.push(slot);
            }
        }
        // A question that moves on while a detail asked for is still missing: ask for that
        // detail again instead (the runtime held the words back, so nothing was said).
        if action == AgentAction::None && spoken.is_empty() {
            if let Some(slot) = self.moves_on(&turn.fields, &asks) {
                tracing::info!(transcript, %slot, ?asks, "the agent moved on past an open question; asking it again");
                self.agent_say(&mut out, None, "", spoken);
                self.ask_again(&mut out, &slot);
                return self.finish(out);
            }
        }

        // Before the engine's own questions below, which replace it.
        self.note_asked(&asks);

        match action {
            // Every detail of a task with no read-back (a price) known: it runs now, whatever the
            // agent chose. A live call heard "רגע, בודק" for a price nothing was asked for, then
            // "הלו? אני פה. רגע, בודק." until the caller hung up.
            AgentAction::None if decided || self.runs_without_read_back() => {
                tracing::info!(transcript, "the task goes on by its own steps");
                self.agent_say(&mut out, None, "", spoken);
                self.advance(&mut out, false);
            }
            AgentAction::None => self.agent_say(&mut out, turn.phrase.as_deref(), &turn.say, spoken),
            AgentAction::Transfer => {
                self.agent_say(&mut out, turn.phrase.as_deref(), &turn.say, spoken);
                self.handoff(&mut out, "caller_requested");
            }
            // "שתיים" to "כמה נוסעים?" was heard "ביי." and the call hung up in the middle of the
            // booking. A lone goodbye there is asked about once: the question again.
            AgentAction::EndCall if self.goodbye_mid_task(transcript) => {
                tracing::info!(
                    transcript,
                    "a lone goodbye in the middle of a booking; maybe misheard, the question again"
                );
                self.state.goodbye_doubted = true;
                self.agent_say(&mut out, None, "", spoken);
                if self.business.response("did_not_catch").is_some() {
                    let ctx = self.render_ctx(None);
                    self.say(&mut out, "did_not_catch", ctx, true);
                }
                let slot = self.expected_slot();
                match slot {
                    Some(slot) => self.ask_again(&mut out, &slot),
                    None => self.read_back(&mut out, true),
                }
            }
            AgentAction::EndCall => {
                // "תודה רבה" to "משהו נוסף?" closes the call as surely as a goodbye.
                if self.caller_said_goodbye(transcript) || (offered_more && self.only_fillers(transcript)) {
                    if spoken.is_empty() && turn.say.is_empty() && turn.phrase.is_none() {
                        self.goodbye(&mut out);
                    } else {
                        self.agent_say(&mut out, turn.phrase.as_deref(), &turn.say, spoken);
                        self.state.phase = Phase::Ending;
                        out.push(Directive::Hangup);
                    }
                } else {
                    // Its goodbye was held back ("תודה, יום טוב!" to a rude remark, in a live
                    // call): go on with the call instead of saying it.
                    tracing::info!(transcript, "agent wanted to end the call without a goodbye; kept it open");
                    self.agent_say(&mut out, None, "", spoken);
                    if self.state.run.is_some() {
                        self.read_back(&mut out, true);
                    } else {
                        let next = if self.state.completed.is_empty() { "small_talk_answer" } else { "anything_else" };
                        if self.business.response(next).is_some() {
                            let ctx = self.render_ctx(None);
                            self.say(&mut out, next, ctx, true);
                        }
                    }
                }
            }
            AgentAction::ReadBack | AgentAction::Submit => {
                // A yes the caller actually said ("כן", "יאללה", "תשלח"): a live call sent a
                // wrong ride on "שעמות", a word recognition made up.
                // "כן אבל אתה יכול רק להחזיר להזמנה" sent a live ride: a yes with a but, a wait or a
                // no in it is a question, and the details are read back again instead.
                // "כן סליחה", "כן, לא צריך כלום": a correction word that corrects nothing does not
                // turn the yes into another read-back.
                let heard = crate::text::normalize(transcript);
                let said_yes = !self.business.affirm.is_empty()
                    && self.business.affirm.find(&heard).is_some()
                    && self.business.correct.find(&self.business.harmless.strip(&heard)).is_none();
                let confirmed_now = action == AgentAction::Submit
                    && !changed
                    && said_yes
                    && self.state.run.as_ref().is_some_and(|r| r.step == Step::AwaitingConfirmation);
                // The read-back asks the question; a question of the agent's own before it
                // would make two ("anything else? ... send it?").
                // Nor a lead-in of its own when the read-back opens with an acknowledgement
                // ("סגור. סגור. ...").
                let read_back_acks = self.state.run.as_ref().is_some_and(|r| {
                    let pipeline = self.pipeline_of(r);
                    let complete = pipeline
                        .slots
                        .iter()
                        .all(|ps| !ps.required || ps.default.is_some() || r.slots.contains_key(&ps.slot));
                    complete
                        && pipeline
                            .confirm
                            .as_ref()
                            .and_then(|c| self.business.response(&c.response))
                            .is_some_and(|resp| resp.prefix.is_some())
                });
                // And on a submit whose action says its own filler ("רגע, בודק"), the agent's
                // "שנייה, אני בודק" would be said twice.
                let action_fills = self.state.run.as_ref().is_some_and(|r| self.pipeline_of(r).filler.is_some());
                let asks =
                    turn.say.trim_end().ends_with('?') || turn.phrase.as_deref().is_some_and(|p| self.phrase_asks(p));
                let quiet = (action == AgentAction::ReadBack && (asks || read_back_acks))
                    || (action == AgentAction::Submit && action_fills);
                let (phrase, say) = if quiet { (None, "") } else { (turn.phrase.as_deref(), turn.say.as_str()) };
                self.agent_say(&mut out, phrase, say, spoken);
                if confirmed_now {
                    if let Some(run) = &mut self.state.run {
                        run.confirmed = true;
                        for s in run.slots.values_mut() {
                            s.confirmed = true;
                            s.provenance = Provenance::Confirmed;
                        }
                    }
                    self.advance(&mut out, false);
                } else {
                    // A submit without a confirmed read-back becomes the read-back. When a
                    // detail is still missing, ask for it unless the agent just asked something.
                    let asked = format!("{spoken} {say}").trim_end().ends_with('?')
                        || (say.is_empty() && phrase.is_some_and(|p| self.phrase_asks(p)));
                    self.read_back(&mut out, !asked);
                }
            }
        }
        if out.pending.is_none() && out.directives.is_empty() && spoken.is_empty() {
            // The model said nothing: never leave the caller in silence. The question again.
            self.question_again(&mut out);
        }
        self.finish(out)
    }

    /// A phrase said on its own (a refusal: "בזה אני לא יכול לעזור, רק במוניות") for words that
    /// name one of the business's own tasks ("כמה זה יוצא לי?"): that task's id. A live call's
    /// price question was refused as off topic.
    pub fn refused_wrongly(&self, transcript: &str, phrase: &str) -> Option<String> {
        if !self.business.response(phrase).is_some_and(|r| r.alone) {
            return None;
        }
        let (u, _) = crate::understanding::fast_path(&self.business, &self.context(), transcript);
        u.intent.map(|i| i.id).filter(|id| self.business.intent(id).is_some_and(|i| i.pipeline.is_some()))
    }

    /// A task whose keywords decide it over the agent's choice (`decisive`), when the caller's
    /// words have one ("עולה נסיעה").
    pub fn decisive_intent(&self, transcript: &str) -> Option<String> {
        let norm = crate::text::normalize(transcript);
        self.business.config.intents.iter().filter(|i| i.decisive).find_map(|i| {
            self.business
                .intent_keywords
                .iter()
                .any(|(id, k)| *id == i.id && k.find(&norm).is_some())
                .then(|| i.id.clone())
        })
    }

    /// The task in progress has no read-back, an action, and every required detail.
    fn runs_without_read_back(&self) -> bool {
        let Some(run) = &self.state.run else { return false };
        if !matches!(run.step, Step::Collecting { .. }) {
            return false;
        }
        let pipeline = self.pipeline_of(run);
        pipeline.confirm.is_none()
            && pipeline.action.is_some()
            && pipeline.slots.iter().all(|ps| !ps.required || ps.default.is_some() || run.slots.contains_key(&ps.slot))
    }

    /// Speak the agent's words: its recorded phrase, then its `say`. When the runtime
    /// already played them, only record what was played, as "the last thing said" for
    /// repeats and context.
    fn agent_say(&mut self, out: &mut Out, phrase: Option<&str>, say: &str, spoken: &str) {
        let delivery = self.state.delivery.clone().unwrap_or_else(|| "normal".into());
        if !spoken.is_empty() {
            let plan = SpeechPlan::free(spoken, &delivery, self.state.gain_db);
            out.recorded = Some(match out.recorded.take() {
                Some(r) => r.then(plan),
                None => plan,
            });
            return;
        }
        if let Some(plan) = phrase.and_then(|p| self.render_phrase(p)) {
            out.speak(plan, true);
        }
        if phrase.is_some_and(|p| !crate::agent::say_after_phrase(&self.business, p, say)) {
            return;
        }
        if !say.trim().is_empty() {
            out.speak(SpeechPlan::free(say, &delivery, self.state.gain_db), true);
        }
    }

    /// Tells the agent, before it decides, of words that sound like a town but are none ("מפרט"
    /// for "מאפרת": a live caller said it three times and was asked about ביתר).
    pub fn hint_towns(&mut self, transcript: &str) {
        let Some(g) = &self.gazetteer else { return };
        // The business's own words are what they are: "מונית" sounds like the kibbutz מענית.
        let known: Vec<String> = self.business.stt_keyterms().iter().map(|t| crate::text::normalize(t)).collect();
        // The business's own towns first, and more loosely: "מלאד" is אלעד where the taxis drive.
        for (heard, town) in g.area_towns_heard(transcript, &self.business.config.service_area) {
            let note = format!(
                "\"{heard}\" is no place, but sounds like {town}, a town this business serves: it is {town}, unless the caller says otherwise"
            );
            if !self.state.agent_notes.contains(&note) {
                self.state.agent_notes.push(note);
            }
        }
        for (heard, town) in g.towns_sounding_like(transcript) {
            if known.iter().any(|k| k.split(' ').any(|w| w == crate::text::normalize(&heard))) {
                continue;
            }
            if self.state.agent_notes.iter().any(|n| n.starts_with(&format!("\"{heard}\""))) {
                continue;
            }
            let note = format!(
                "\"{heard}\" is no place, but sounds like {town} (the same consonants): if that fits, it is {town}"
            );
            if !self.state.agent_notes.contains(&note) {
                self.state.agent_notes.push(note);
            }
        }
    }

    /// A place's city as understood. Another city than before is a correction ("לא ברקת,
    /// בני ברק"): what was doubted about a street of the other city no longer holds.
    fn note_city(&mut self, slot: &str, city: String) {
        if self.state.place_cities.get(slot).is_some_and(|before| *before != city) {
            self.state.doubted_streets.remove(slot);
            self.state.doubt_confirm.remove(slot);
            self.state.place_numbers.remove(slot);
            self.state.number_asked.remove(slot);
        }
        self.state.place_cities.insert(slot.to_string(), city);
    }

    /// The response that asks for the house number of a place slot: `<its ask>_number`.
    fn number_question(&self, slot: &str) -> String {
        format!("{}_number", self.pipeline_ask(slot).unwrap_or_default())
    }

    fn pipeline_ask(&self, slot: &str) -> Option<String> {
        let run = self.state.run.as_ref()?;
        self.pipeline_of(run).slots.iter().find(|ps| ps.slot == slot).and_then(|ps| ps.ask.clone())
    }

    /// A city given for a place that holds a value from elsewhere ("אפרק", or another city):
    /// the old value goes, so the place is asked for again in the city given. A live call kept
    /// "אפרק" as the pickup after the caller had said "מאפרת".
    fn drop_stale_place(&mut self, slot: &str, city: &str) {
        let Some(g) = self.gazetteer.clone() else { return };
        let Some(run) = self.state.run.as_mut() else { return };
        let stale = run.slots.get(slot).is_some_and(|s| match &s.value {
            SlotValue::Place { spoken, .. } => !matches!(g.resolve(spoken), Lookup::Found(a) if a.city_said == city),
            _ => false,
        });
        if stale {
            run.slots.remove(slot);
            self.state.place_numbers.remove(slot);
            self.state.number_asked.remove(slot);
        }
    }

    /// The response that asks for the city of a place slot: `<its ask>_city`.
    fn city_question(&self, slot: &str) -> String {
        let run = self.state.run.as_ref();
        let ask =
            run.and_then(|r| self.pipeline_of(r).slots.iter().find(|ps| ps.slot == slot).and_then(|ps| ps.ask.clone()));
        format!("{}_city", ask.unwrap_or_default())
    }

    /// The response that asks for the street of a place slot: `<its ask>_street`
    /// ("ask_destination_street").
    fn street_question(&self, slot: &str) -> String {
        let ask =
            self.state.run.as_ref().and_then(|r| {
                self.pipeline_of(r).slots.iter().find(|ps| ps.slot == slot).and_then(|ps| ps.ask.clone())
            });
        format!("{}_street", ask.unwrap_or_default())
    }

    /// How a note to the agent names one of the business's responses: by phrase id when it
    /// is one of the agent's recorded phrases, else by its wording; nothing when it has none.
    fn said_as(&self, response_id: &str) -> String {
        self.said_as_in(response_id, None)
    }

    /// The same, with a question about a place in a city worded for that city ("לאן בבני ברק?").
    fn said_as_in(&self, response_id: &str, city: Option<&str>) -> String {
        if crate::agent::phrase_ids(&self.business).contains(&response_id) {
            return format!(" (phrase {response_id})");
        }
        self.business
            .response(response_id)
            .and_then(|r| r.variants.first())
            .map(|v| format!(" (\"{}\")", v.replace("{city}", city.unwrap_or("<the city>"))))
            .unwrap_or_default()
    }

    /// The slot of the current task a phrase asks for: its `ask`, or that ask's street or
    /// city question ("ask_destination_street" asks for the destination).
    pub fn slot_asked_by(&self, phrase: &str) -> Option<String> {
        let run = self.state.run.as_ref()?;
        self.pipeline_of(run)
            .slots
            .iter()
            .find(|ps| {
                ps.ask.as_deref().is_some_and(|a| {
                    [a.to_string(), format!("{a}_street"), format!("{a}_city"), format!("{a}_number")]
                        .contains(&phrase.to_string())
                })
            })
            .map(|ps| ps.slot.clone())
    }

    /// The task's next question, in its order (what the agent is told under NOW), or the
    /// read-back when nothing is left to ask.
    fn next_question(&mut self, out: &mut Out) {
        let Some(run) = &self.state.run else { return };
        let pipeline = self.pipeline_of(run).clone();
        self.fill_defaults(&pipeline);
        let Some(run) = &self.state.run else { return };
        let next = pipeline.slots.iter().find(|ps| {
            !run.slots.contains_key(&ps.slot)
                && ps.default.is_none()
                && (ps.required || ps.ask.is_some())
                && !(ps.ask_before_confirm && self.state.asked_before_confirm.contains(&ps.slot))
                // In a fixed order, an optional question is asked once ("על שם מי?" "לא משנה").
                && !(pipeline.strict_order && !ps.required && self.state.asked_slots.contains(&ps.slot))
        });
        match next.map(|ps| (ps.slot.clone(), ps.ask_before_confirm)) {
            Some((slot, before_confirm)) => {
                if before_confirm {
                    self.state.asked_before_confirm.insert(slot.clone());
                }
                self.ask(out, &slot, true);
            }
            None => self.read_back(out, true),
        }
    }

    /// The recorded phrase is a question ("לאיזה רחוב?").
    fn phrase_asks(&self, id: &str) -> bool {
        self.business.response(id).and_then(|r| r.variants.first()).is_some_and(|v| v.trim_end().ends_with('?'))
    }

    /// Values the agent heard, through the same parsers as the fast path. Returns whether
    /// any value of the current task changed.
    /// Values for details already given, passed while the question was about something
    /// else, are not taken unless the caller is correcting ("לא, ל..."): in a live call the
    /// answer to "על שם מי לרשום?" replaced the destination with a street of the wrong city.
    fn answer_in_place(&mut self, transcript: &str, fields: &[(String, String)]) -> Vec<(String, String)> {
        let asked = self.state.last_asks.clone();
        let Some(run) = self.state.run.as_ref() else { return fields.to_vec() };
        if asked.is_empty()
            || !matches!(run.step, Step::Collecting { .. })
            || self.business.correct.find(&crate::text::normalize(transcript)).is_some()
        {
            return fields.to_vec();
        }
        let mut kept = Vec::new();
        let mut notes = Vec::new();
        for (slot, raw) in fields {
            match run.slots.get(slot) {
                Some(current) if !asked.contains(slot) => {
                    let current = current.value.spoken();
                    let same = crate::text::normalize(&current).contains(&crate::text::normalize(raw));
                    if !same {
                        tracing::info!(transcript, %slot, raw, %current, ?asked, "a value for a detail not asked about; kept the one given");
                        notes.push(format!(
                            "{slot} is already \"{current}\" and your question was about {}: it was not changed.                              Change a detail only when the caller corrects it.",
                            asked.join(", ")
                        ));
                    }
                }
                _ => kept.push((slot.clone(), raw.clone())),
            }
        }
        self.state.agent_notes.extend(notes);
        kept
    }

    /// A place passed for the pickup that the caller said with the destination's "ל" ("לביתר"),
    /// or the other way round: it is the other place (a live call took "מאפרת לביתר" for a
    /// pickup in ביתר). Only when the reply does not pass the other place itself.
    fn by_preposition(&self, transcript: &str, fields: &[(String, String)]) -> Vec<(String, String)> {
        let heard = crate::text::normalize(transcript);
        let words: Vec<&str> = heard.split(' ').filter(|w| !w.is_empty()).collect();
        let prefixes = |slot: &str| -> Vec<String> {
            self.business.config.slots.get(slot).map(|c| c.strip_prefixes.clone()).unwrap_or_default()
        };
        // Whether the place's first word was said with one of these prefixes before it.
        let said_with = |value: &str, with: &[String]| {
            let first = crate::text::normalize(value.split(',').next().unwrap_or(value));
            let Some(first) = first.split(' ').next().filter(|w| w.chars().count() >= 2) else { return false };
            words.iter().any(|w| with.iter().any(|p| w.strip_prefix(p.as_str()) == Some(first)))
        };
        let places: Vec<&str> = ["pickup", "destination"]
            .into_iter()
            .filter(|s| self.business.config.slots.get(*s).is_some_and(|c| c.kind == crate::config::SlotKind::Place))
            .collect();
        if places.len() != 2 {
            return fields.to_vec();
        }
        fields
            .iter()
            .map(|(slot, value)| {
                let Some(other) = places.iter().find(|p| **p != slot && places.contains(&slot.as_str())) else {
                    return (slot.clone(), value.clone());
                };
                let (own, theirs) = (prefixes(slot), prefixes(other));
                let moves =
                    !fields.iter().any(|(s, _)| s == other) && said_with(value, &theirs) && !said_with(value, &own);
                if moves {
                    tracing::info!(%slot, %value, to = other, "said with the other place's preposition; moved");
                    (other.to_string(), value.clone())
                } else {
                    (slot.clone(), value.clone())
                }
            })
            .collect()
    }

    /// The street asked for ("איפה בביתר עילית לאסוף?"), answered in a word or two the agent
    /// passed nothing for ("אהרן"): that answer, in that city, so it is looked up (and the
    /// closest street offered) instead of the question asked again. A live call heard "רק לוודא
    /// שאין טעות, איזה רחוב?" and answered "אהרן" twice. Not a yes or no, a hello, a
    /// question or noise.
    fn street_answer(&self, transcript: &str, turn: &AgentTurn) -> Vec<(String, String)> {
        let fields = turn.fields.clone();
        let Some(run) = &self.state.run else { return fields };
        if !fields.is_empty() || turn.action != AgentAction::None || !matches!(run.step, Step::Collecting { .. }) {
            return fields;
        }
        let Some(slot) = self.state.last_asks.iter().find(|s| {
            self.business.config.slots.get(*s).is_some_and(|c| c.kind == crate::config::SlotKind::Place)
                && !run.slots.contains_key(*s)
                && self.state.place_cities.contains_key(*s)
        }) else {
            return fields;
        };
        let words = crate::text::normalize(transcript);
        let rest = self.business.fillers.strip(&words);
        let count = rest.split_whitespace().count();
        let (u, _) = crate::understanding::fast_path(&self.business, &self.context(), transcript);
        if !(1..=4).contains(&count)
            || transcript.trim_end().ends_with('?')
            || u.noise
            || u.meta.is_some()
            || self.is_hello(transcript)
            || self.business.affirm.find(&words).is_some()
            || self.business.deny.find(&words).is_some()
        {
            return fields;
        }
        let city = &self.state.place_cities[slot];
        let value = format!("{}, {city}", transcript.trim().trim_end_matches(['.', '!', ',']));
        tracing::info!(%slot, %value, "the street asked for, answered without the agent passing it");
        vec![(slot.clone(), value)]
    }

    /// A place that is only a city, given to the wrong one of two places: the caller now says it
    /// with the other's preposition, beside another place said with this one's ("מלעד
    /// לירושלים" after ירושלים was taken for the pickup). It is cleared, so it is asked for
    /// again: a live ride went out from בן זכאי 45 in ירושלים instead of אלעד.
    fn misplaced_city(&mut self, transcript: &str) {
        let (Some(g), Some(run)) = (&self.gazetteer, &self.state.run) else { return };
        if !matches!(run.step, Step::Collecting { .. }) {
            return;
        }
        let heard = crate::text::normalize(transcript);
        let words: Vec<&str> = heard.split(' ').filter(|w| !w.is_empty()).collect();
        let prefixes = |slot: &str| -> Vec<String> {
            self.business.config.slots.get(slot).map(|c| c.strip_prefixes.clone()).unwrap_or_default()
        };
        let mut wrong = None;
        for (slot, other) in [("pickup", "destination"), ("destination", "pickup")] {
            // The city alone: as the value, or (a street still to come) as its city so far.
            let city_said = match run.slots.get(slot).map(|s| &s.value) {
                Some(SlotValue::Place { spoken, .. }) => match g.resolve(spoken) {
                    Lookup::Found(a) if a.street.is_none() && a.place.is_none() => a.city_said,
                    _ => continue,
                },
                Some(_) => continue,
                None => match self.state.place_cities.get(slot) {
                    Some(city) => city.clone(),
                    None => continue,
                },
            };
            let city = crate::text::normalize(&city_said);
            let Some(first) = city.split(' ').next() else { continue };
            let (own, theirs) = (prefixes(slot), prefixes(other));
            let with =
                |ps: &[String]| words.iter().any(|w| ps.iter().any(|p| w.strip_prefix(p.as_str()) == Some(first)));
            // And another place said with this one's preposition ("מלעד").
            let another = words.iter().any(|w| {
                own.iter()
                    .any(|p| w.strip_prefix(p.as_str()).is_some_and(|rest| rest.chars().count() >= 2 && rest != first))
            });
            if with(&theirs) && !with(&own) && another {
                wrong = Some((slot.to_string(), city_said.clone(), other.to_string()));
                break;
            }
        }
        let Some((slot, city, other)) = wrong else { return };
        tracing::info!(%slot, %city, transcript, "a city said with the other place's preposition; cleared");
        if let Some(run) = &mut self.state.run {
            run.clear(&slot);
        }
        self.state.place_cities.remove(&slot);
        self.state.agent_notes.push(format!(
            "{slot}: {city} is the {other} (the caller said it so); {slot} is the other place they said"
        ));
    }

    /// Details the caller said in words the business's patterns know ("אנחנו שלושה", "השארתי
    /// תיק") that the agent left out of its reply: taken too, so no detail said is lost and
    /// asked for again. Not a place that needs its street (the agent and the street list read
    /// those; a city is enough for a price: "לנתב״ג"), not a bare answer, and only while
    /// details are being collected.
    fn with_patterns(&self, transcript: &str, fields: &[(String, String)]) -> Vec<(String, String)> {
        let Some(run) = &self.state.run else { return fields.to_vec() };
        if !matches!(run.step, Step::Collecting { .. }) {
            return fields.to_vec();
        }
        let pipeline = self.pipeline_of(run);
        let ctx = crate::understanding::Context { awaiting_slot: None, awaiting_confirmation: false, ..self.context() };
        let (u, _) = crate::understanding::fast_path(&self.business, &ctx, transcript);
        let mut out = fields.to_vec();
        for mut fill in u.slots {
            // Read under the name of the detail this task takes it from ("destination" for a
            // price question's "price_to").
            if !pipeline.slots.iter().any(|ps| ps.slot == fill.slot) {
                if let Some(t) = pipeline.slots.iter().find(|ps| ps.from_slot.as_deref() == Some(fill.slot.as_str())) {
                    fill.slot = t.slot.clone();
                }
            }
            let Some(cfg) = self.business.config.slots.get(&fill.slot) else { continue };
            // A place that needs its street: only a town the list knows exactly ("מביתר", "לירושלים"),
            // its first step; the street is asked next.
            let street_needed = cfg.kind == crate::config::SlotKind::Place && (cfg.precise || cfg.street_once);
            let a_town = || {
                self.gazetteer.as_ref().is_some_and(|g| {
                    matches!(g.resolve(&fill.value.spoken()), crate::gazetteer::Lookup::Found(a) if a.street.is_none() && a.number.is_none() && a.place.is_none())
                })
            };
            let wanted = (!street_needed || a_town())
                && !cfg.patterns.is_empty()
                && pipeline.slots.iter().any(|ps| ps.slot == fill.slot)
                && !run.slots.contains_key(&fill.slot)
                && !out.iter().any(|(s, _)| *s == fill.slot);
            if wanted {
                tracing::info!(slot = %fill.slot, value = %fill.value.spoken(), "said, and left out by the agent; taken");
                out.push((fill.slot.clone(), fill.value.spoken()));
            }
        }
        out
    }

    /// Details with cue words (`cues`) given instead of the answer to a question about
    /// something else, without those words: a mishearing of that answer, not the detail.
    /// Only while details are collected; a caller's first sentence ("לירושלים, שלושה") and a
    /// detail added to the answer ("לעזריאלי, שניים") are taken as they are.
    fn with_cues(&self, transcript: &str, fields: &[(String, String)]) -> Vec<(String, String)> {
        let Some(run) = &self.state.run else { return fields.to_vec() };
        let answered = fields.iter().any(|(s, v)| self.state.last_asks.contains(s) && !v.trim().is_empty());
        if !matches!(run.step, Step::Collecting { .. }) || self.state.last_asks.is_empty() || answered {
            return fields.to_vec();
        }
        let heard = crate::text::normalize(transcript);
        fields
            .iter()
            .filter(|(slot, value)| {
                let Some(cfg) = self.business.config.slots.get(slot) else { return true };
                if cfg.cues.is_empty() || self.state.last_asks.contains(slot) || value.trim().is_empty() {
                    return true;
                }
                let said = cfg.cues.iter().any(|c| heard.contains(&crate::text::normalize(c)));
                if !said {
                    tracing::info!(%slot, %value, transcript, "a detail not asked for, said without its words; not taken");
                }
                said
            })
            .cloned()
            .collect()
    }

    /// A value passed under the name of the detail another task takes it from ("destination"
    /// in a price question, whose "price_to" takes it from there): it is that detail.
    fn as_this_tasks(&self, fields: &[(String, String)]) -> Vec<(String, String)> {
        let Some(run) = &self.state.run else { return fields.to_vec() };
        let pipeline = self.pipeline_of(run);
        fields
            .iter()
            .map(|(slot, value)| {
                let own = pipeline.slots.iter().any(|ps| ps.slot == *slot);
                let target = pipeline.slots.iter().find(|ps| ps.from_slot.as_deref() == Some(slot.as_str()));
                match target {
                    Some(t) if !own && !fields.iter().any(|(s, _)| *s == t.slot) => (t.slot.clone(), value.clone()),
                    _ => (slot.clone(), value.clone()),
                }
            })
            .collect()
    }

    /// A number given for `slot` that is more than the slot allows ("מאה" passengers).
    fn above_max(&self, slot: &str, fields: &[(String, String)]) -> bool {
        let Some(max) = self.business.config.slots.get(slot).and_then(|c| c.max) else { return false };
        fields.iter().filter(|(s, _)| s == slot).any(|(_, v)| {
            let norm = crate::text::normalize(v);
            let toks = crate::text::tokens(&norm);
            // Thousands are not read as numbers; they are past any maximum anyway.
            toks.iter().any(|t| matches!(*t, "אלף" | "אלפיים" | "אלפים" | "מיליון"))
                || crate::hebrew::find_numbers(&toks).first().is_some_and(|n| n.value > max)
        })
    }

    fn apply_agent_fields(&mut self, fields: &[(String, String)]) -> (bool, Vec<String>) {
        let fields = &self.as_this_tasks(fields);
        let customer = self.state.customer.clone();
        let mut notes = Vec::new();
        let mut rejected = Vec::new();
        // Everything the caller has said in this call, to check places against.
        let heard: String = self
            .state
            .history
            .iter()
            .filter(|t| t.speaker == Speaker::Caller)
            .map(|t| t.text.as_str())
            .collect::<Vec<_>>()
            .join(". ");
        // What the second hearing heard counts as said too, and the streets the system offered
        // (a "yes" to "התכוונת לרבן יוחנן בן זכאי?" was refused as words never said).
        let heard = match &self.state.second_hearing {
            Some(second) => format!("{heard}. {second}"),
            None => heard,
        };
        let heard = if self.state.offered_streets.is_empty() {
            heard
        } else {
            format!("{heard}. {}", self.state.offered_streets.join(". "))
        };
        // And the cities it understood from them: "מביתר" is ביתר עילית, and the agent writing it
        // in full ("הרמב״ן 16, ביתר עילית") made up no word ("עילית" was refused).
        let mut understood: Vec<String> = self.state.place_cities.values().cloned().collect();
        // And the places of the customer's record ("הבית").
        if let Some(c) = &self.state.customer {
            understood.extend(c.places.values().flat_map(|p| [Some(p.spoken.clone()), p.address.clone()]).flatten());
        }
        // And the full names of the towns the caller named now: "מביתר" is ביתר עילית.
        if let Some(g) = &self.gazetteer {
            understood.extend(g.towns_named(&heard));
            understood
                .extend(g.area_towns_heard(&heard, &self.business.config.service_area).into_iter().map(|(_, t)| t));
        }
        let heard = if understood.is_empty() { heard } else { format!("{heard}. {}", understood.join(". ")) };
        // The optional questions asked before the read-back ("יש משהו שהנהג צריך לדעת?"): a "no"
        // to one is its answer, not its value (a live ride went out with the note "לא").
        let optional: Vec<String> = self
            .state
            .run
            .as_ref()
            .map(|r| {
                self.pipeline_of(r)
                    .slots
                    .iter()
                    .filter(|ps| ps.ask_before_confirm && !ps.required)
                    .map(|ps| ps.slot.clone())
                    .collect()
            })
            .unwrap_or_default();
        let said_now = self
            .state
            .history
            .iter()
            .rev()
            .find(|t| t.speaker == Speaker::Caller)
            .map(|t| t.text.clone())
            .unwrap_or_default();
        let fills: Vec<SlotFill> = fields
            .iter()
            .filter_map(|(slot, raw)| {
                let cfg = self.business.config.slots.get(slot)?;
                let corrected = (cfg.kind == crate::config::SlotKind::Place)
                    .then(|| self.gazetteer.as_ref().and_then(|g| g.street_said_instead(raw, &said_now)))
                    .flatten();
                if let Some(c) = &corrected {
                    tracing::info!(%slot, agent = %raw, caller = %c, "the street the caller said, not the agent's");
                }
                let raw = corrected.as_deref().unwrap_or(raw);
                if optional.contains(slot) {
                    let norm = crate::text::normalize(raw);
                    let rest =
                        self.business.fillers.strip(&self.business.nothing.strip(&self.business.deny.strip(&norm)));
                    if !norm.trim().is_empty() && rest.trim().is_empty() {
                        self.state.asked_before_confirm.insert(slot.clone());
                        return None;
                    }
                }
                if cfg.kind == crate::config::SlotKind::Place {
                    let invented = crate::gazetteer::unheard_words(raw, &heard);
                    let rejections = self.state.unheard_rejections.get(slot).copied().unwrap_or(0);
                    if !invented.is_empty() && rejections < 2 {
                        *self.state.unheard_rejections.entry(slot.clone()).or_default() += 1;
                        notes.push(format!(
                            "{slot}: the caller never said \"{}\"; do not guess, ask for the street name",
                            invented.join(" ")
                        ));
                        rejected.push(slot.clone());
                        return None;
                    }
                }
                let parsed = parse_slot_value(&self.business, slot, cfg, raw, true, customer.as_ref());
                // The parser marks impossible values (42 passengers when the most is 20)
                // below `reject_below`; the agent's certainty does not make them possible.
                let Some((value, confidence)) = parsed.filter(|(_, c)| *c >= cfg.reject_below) else {
                    // Not the limits: given the range, the agent lectured about it every turn.
                    notes.push(format!("{slot} \"{raw}\" was not accepted; ask for it again with the short question"));
                    rejected.push(slot.clone());
                    return None;
                };
                let value = self.check_place(slot, value, raw, &mut notes, &mut rejected)?;
                Some(SlotFill {
                    slot: slot.clone(),
                    value,
                    confidence: confidence.max(0.9),
                    provenance: Provenance::Llm,
                })
            })
            .collect();
        self.state.agent_notes.extend(notes);
        if fills.is_empty() {
            return (false, rejected);
        }
        if self.state.run.is_none() {
            if let Some((pipeline, intent)) = self.infer_pipeline(&fills) {
                self.state.run = Some(self.new_run(&pipeline, &intent));
            }
        }
        let Some(run) = &self.state.run else { return (false, rejected) };
        let before = run.slots.clone();
        self.apply_fills(&fills);
        let changed = self.state.run.as_ref().is_some_and(|r| r.slots != before);
        if changed {
            if let Some(run) = &mut self.state.run {
                if run.step == Step::AwaitingConfirmation {
                    run.step = Step::Collecting { awaiting: None };
                }
            }
        }
        (changed, rejected)
    }

    /// Defaults and customer-known values for anything still missing.
    /// Details this task takes from another task of the call (`from_slot`): the booking under
    /// way (suspended), then tasks done, then a place of which only the city is known yet.
    fn fill_from_other_tasks(&mut self) {
        let Some(run) = &self.state.run else { return };
        let pipeline = self.pipeline_of(run).clone();
        let mut found: Vec<(String, SlotValue)> = Vec::new();
        for ps in &pipeline.slots {
            let Some(source) = &ps.from_slot else { continue };
            if run.slots.contains_key(&ps.slot) {
                continue;
            }
            let value = self
                .state
                .suspended
                .iter()
                .rev()
                .find_map(|r| r.slots.get(source).map(|s| s.value.clone()))
                .or_else(|| self.state.completed.iter().rev().find_map(|c| c.slots.get(source).cloned()))
                .or_else(|| {
                    self.state.place_cities.get(source).map(|city| SlotValue::Place {
                        spoken: city.clone(),
                        address: None,
                        customer_place: None,
                    })
                });
            if let Some(v) = value {
                found.push((ps.slot.clone(), v));
            }
        }
        if let Some(run) = &mut self.state.run {
            for (slot, value) in found {
                tracing::info!(%slot, value = %value.spoken(), "taken from another task of the call");
                run.slots.insert(
                    slot,
                    SlotState { value, confidence: 0.9, provenance: Provenance::Confirmed, confirmed: false },
                );
            }
        }
    }

    /// What the customer record gives a detail (`from_customer`): a saved place ("home"), or the
    /// customer's name ("name").
    fn customer_gives(&self, ps: &crate::config::PipelineSlot) -> Option<SlotValue> {
        let (key, c) = (ps.from_customer.as_ref()?, self.state.customer.as_ref()?);
        if key == "name" {
            return c.name.clone().filter(|n| !n.trim().is_empty()).map(|text| SlotValue::Text { text });
        }
        let place = c.places.get(key)?;
        Some(SlotValue::Place {
            spoken: place.spoken.clone(),
            address: place.address.clone(),
            customer_place: Some(key.clone()),
        })
    }

    fn fill_defaults(&mut self, pipeline: &PipelineConfig) {
        self.fill_from_other_tasks();
        let known: Vec<(String, SlotValue)> = pipeline
            .slots
            .iter()
            .filter(|ps| ps.from_customer.as_deref() == Some("name"))
            .filter_map(|ps| self.customer_gives(ps).map(|v| (ps.slot.clone(), v)))
            .collect();
        if let Some(run) = &mut self.state.run {
            for (slot, value) in known {
                run.slots.entry(slot).or_insert(SlotState {
                    value,
                    confidence: 0.9,
                    provenance: Provenance::Customer,
                    confirmed: false,
                });
            }
        }
        let customer = self.state.customer.clone();
        let Some(run) = &mut self.state.run else { return };
        for ps in &pipeline.slots {
            if run.slots.contains_key(&ps.slot) {
                continue;
            }
            if let (Some(key), Some(c)) = (&ps.from_customer, &customer) {
                if let Some(place) = c.places.get(key) {
                    run.slots.insert(
                        ps.slot.clone(),
                        SlotState {
                            value: SlotValue::Place {
                                spoken: place.spoken.clone(),
                                address: place.address.clone(),
                                customer_place: Some(key.clone()),
                            },
                            confidence: 0.8,
                            provenance: Provenance::Customer,
                            confirmed: false,
                        },
                    );
                    continue;
                }
            }
            if let Some(default) = &ps.default {
                if let Some(v) = default_value(&self.business, &ps.slot, default) {
                    run.slots.insert(
                        ps.slot.clone(),
                        SlotState { value: v, confidence: 1.0, provenance: Provenance::Default, confirmed: false },
                    );
                }
            }
        }
    }

    /// A place the business does not know itself, checked against Israel's localities and
    /// streets: stored in its official spelling when found, and reported to the agent (with
    /// the closest names) when not, so it asks the caller rather than guessing.
    /// `None` when the place cannot be used as it is: a city alone for a precise slot (progress,
    /// the street comes next) or a street the city does not have (`rejected`).
    fn check_place(
        &mut self,
        slot: &str,
        value: SlotValue,
        raw: &str,
        notes: &mut Vec<String>,
        rejected: &mut Vec<String>,
    ) -> Option<SlotValue> {
        let (Some(g), SlotValue::Place { spoken, address: None, customer_place: None }) = (&self.gazetteer, &value)
        else {
            return Some(value);
        };
        let precise = self.business.config.slots.get(slot).is_some_and(|c| c.precise);
        let street_once = self.business.config.slots.get(slot).is_some_and(|c| c.street_once);
        // A street given after its city ("מאיזו עיר?" "אלעד" ... "איזה רחוב?" "בן זכאי 45"):
        // look it up in the city given before, for this slot.
        // Or of the value it corrects ("לא 40, 45" after "רבן יוחנן בן זכאי 40, אלעד").
        let city_before = self.state.place_cities.get(slot).cloned().or_else(|| {
            self.state.run.as_ref().and_then(|r| r.slots.get(slot)).and_then(|s| match &s.value {
                SlotValue::Place { spoken, .. } => match g.resolve(spoken) {
                    Lookup::Found(a) => Some(a.city_said),
                    _ => None,
                },
                _ => None,
            })
        });
        // With the city given before, try the street there first ("בן זכאי 45" after "אלעד"
        // is a street of אלעד, not the moshav בן זכאי).
        let in_city_before = city_before
            .as_ref()
            .map(|city| g.resolve(&format!("{spoken}, {city}")))
            .filter(|l| matches!(l, Lookup::Found(a) if a.street.is_some()));
        // "street, city" as the agent wrote it (the parsed value has lost the comma): the
        // city after the comma, the street before.
        let explicit = raw.rsplit_once(',').and_then(|(street, city)| g.resolve_within(street, city.trim()));
        let explicit_given = explicit.is_some();
        let mut lookup = explicit.or(in_city_before).unwrap_or_else(|| g.resolve(spoken));
        // "מודיעין עילית" said alone lost its "מ" as if it were "from" ("ודיעין עילית"): with
        // no such place, the place with the letter back.
        if matches!(lookup, Lookup::NoCity { .. }) {
            let prefixes = self.business.config.slots.get(slot).map(|c| c.strip_prefixes.clone()).unwrap_or_default();
            if let Some(found) =
                prefixes.iter().map(|p| g.resolve(&format!("{p}{spoken}"))).find(|l| matches!(l, Lookup::Found(_)))
            {
                lookup = found;
            }
        }
        // The street as the caller said it: this answer's, or the one given before when this
        // answer is only its city.
        let mut said = raw.to_string();
        // The city for a street given before without one ("בן זכאי 45" ... "אלעד").
        if let (Lookup::Found(a), Some(street)) = (&lookup, self.state.place_streets.get(slot)) {
            if a.street.is_none() && a.number.is_none() {
                if let Some(joined) = g.resolve_within(street, &a.city_said) {
                    said = street.clone();
                    lookup = joined;
                }
            }
        }
        // "בית דחה 45" after "אלעד": no locality in the words, so they are looked up in the city
        // given before (and found or not found there), never taken for a place with no city.
        if let (Lookup::NoCity { .. }, Some(city)) = (&lookup, city_before.as_ref()) {
            if let Some(within) = g.resolve_within(spoken, city) {
                lookup = within;
            }
        }
        // Two misses and no desk to hand the call to: the third time the place is taken as the
        // caller says it, marked for the driver, rather than a fourth question or a hangup.
        let give_up =
            self.business.handoff_number.is_none() && self.state.place_rejections.get(slot).copied().unwrap_or(0) >= 2;
        // The house number for a street given before without one ("בן זכאי" ... "45").
        if let Some((street, city)) = self.state.place_numbers.get(slot).cloned() {
            let number = spoken.trim();
            if !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()) {
                if let Some(joined) = g.resolve_within(&format!("{street} {number}"), &city) {
                    lookup = joined;
                }
            }
        }
        // "הנביאים 2" not in בני ברק, then "בירושלים": the street there, with the number said before.
        if let (Lookup::Found(a), Some(number)) = (&lookup, self.state.doubted_numbers.get(slot).cloned()) {
            if a.street.is_some() && a.number.is_none() && a.place.is_none() {
                let street = a.street_said.clone().or_else(|| a.street.clone()).unwrap_or_default();
                if let Some(joined) = g.resolve_within(&format!("{street} {number}"), &a.city_said) {
                    if matches!(&joined, Lookup::Found(j) if j.number.is_some()) {
                        lookup = joined;
                    }
                }
            }
        }
        if matches!(&lookup, Lookup::Found(a) if a.number.is_some()) {
            self.state.doubted_numbers.remove(slot);
        }
        // A street only the second hearing heard: the stream's words do not sound like any of
        // its names ("עפרה." heard again as שדרות כפר עציון, "עריף" as ראב"ד). The audio
        // model, told a city's streets, sometimes forces one onto whatever was said.
        let unheard_street = match &lookup {
            Lookup::Found(a)
                if a.place.is_none()
                    && self.state.second_hearing.is_some()
                    && !self.state.unheard_streets.contains(slot) =>
            {
                let stream: String = self
                    .state
                    .history
                    .iter()
                    .filter(|t| t.speaker == Speaker::Caller)
                    .map(|t| t.text.as_str())
                    .chain(self.state.offered_streets.iter().map(String::as_str))
                    .collect::<Vec<_>>()
                    .join(". ");
                a.street
                    .as_ref()
                    .filter(|s| !g.street_heard(&a.city, s, &stream))
                    .map(|_| a.street_said.clone().or_else(|| a.street.clone()).unwrap_or_default())
            }
            _ => None,
        };
        Some(match lookup {
            Lookup::Found(a) if unheard_street.is_some() && self.business.response("street_heard_check").is_some() => {
                let street = unheard_street.unwrap_or_default();
                let number = a.number.clone().unwrap_or_default();
                let named = format!("{street} {number}").trim().to_string();
                notes.push(format!(
                    "{slot}: only the second hearing heard the street {street}; the caller's words do not sound like                      it, so it was not taken. The system asked whether they said it: if they agree, pass \"{named}, {}\";                      if not, ask for the street again",
                    a.city_said
                ));
                self.state.unheard_streets.insert(slot.to_string());
                if !self.state.offered_streets.contains(&street) {
                    self.state.offered_streets.push(street);
                }
                if !number.is_empty() {
                    self.state.doubted_numbers.insert(slot.to_string(), number);
                }
                self.state.doubt_confirm.insert(
                    slot.to_string(),
                    vec![prompt("street_heard_check", &[("street", named), ("city", a.city_said.clone())])],
                );
                self.state.place_cities.insert(slot.to_string(), a.city_said);
                rejected.push(slot.to_string());
                return None;
            }
            // "בן זכאי 45" with no city: a house number is a street's, and the locality its words
            // name (the moshav בן זכאי) is not where the caller is. Ask for the city and keep
            // the street for it.
            Lookup::Found(a)
                if a.street.is_none() && a.number.is_some() && city_before.is_none() && !explicit_given =>
            {
                let ask = self.said_as(&self.city_question(slot));
                notes.push(format!(
                    "{slot} \"{spoken}\" is a street and number without its city (not the locality {}); ask which \
                     city{ask}, and pass the street with it",
                    a.city
                ));
                self.state.place_streets.insert(slot.to_string(), spoken.clone());
                rejected.push(slot.to_string());
                return None;
            }
            // Asked once already, and the caller has no street: the locality is enough.
            Lookup::Found(a)
                if street_once
                    && !precise
                    && a.street.is_none()
                    && self.state.place_cities.get(slot) != Some(&a.city_said) =>
            {
                let ask = self.said_as_in(&self.street_question(slot), Some(&a.city_said));
                notes.push(format!(
                    "{slot} city {} is noted; now ask for the street there, once{ask}; if the caller does not know, \
                     pass the city again",
                    a.city_said
                ));
                self.drop_stale_place(slot, &a.city_said);
                self.note_city(slot, a.city_said);
                return None;
            }
            Lookup::Found(a) if precise && a.street.is_none() => {
                let ask = self.said_as_in(&self.street_question(slot), Some(&a.city_said));
                notes.push(format!(
                    "{slot} city {} is noted; now ask for the street and house number (or a landmark) there{ask}",
                    a.city_said
                ));
                self.drop_stale_place(slot, &a.city_said);
                self.note_city(slot, a.city_said);
                return None;
            }
            // A street without its house number: the number is asked on its own, once, so the
            // caller answers one thing at a time; "לא יודע" (the street again) takes the street.
            Lookup::Found(a)
                if a.street.is_some()
                    && a.number.is_none()
                    && a.place.is_none()
                    && !self.state.number_asked.contains(slot)
                    && self.business.response(&self.number_question(slot)).is_some() =>
            {
                let street = a.street_said.clone().or_else(|| a.street.clone()).unwrap_or_default();
                let ask = self.said_as(&self.number_question(slot));
                notes.push(format!(
                    "{slot}: street {street} in {} is noted; now ask for the house number there{ask}; if the caller \
                     does not know it, pass the street again",
                    a.city_said
                ));
                self.state.number_asked.insert(slot.to_string());
                self.state.place_numbers.insert(slot.to_string(), (street, a.city_said));
                self.state.place_cities.remove(slot);
                rejected.push(slot.to_string());
                return None;
            }
            Lookup::Found(a) => {
                self.state.place_cities.remove(slot);
                self.state.place_streets.remove(slot);
                self.state.place_numbers.remove(slot);
                self.state.doubt_confirm.remove(slot);
                self.state.place_rejections.remove(slot);
                SlotValue::Place { spoken: a.spoken(), address: Some(a.official()), customer_place: None }
            }
            // "אפרק": no locality by that name. A place that needs one is not taken as it was
            // heard (a live ride was booked from "אפרק"): the caller is asked which city.
            Lookup::NoCity { closest } if (precise || street_once) && !give_up => {
                let hint = if closest.is_empty() {
                    String::new()
                } else {
                    format!(" (closest localities: {}; if the caller meant one, confirm it)", closest.join(", "))
                };
                notes.push(format!(
                    "{slot} \"{spoken}\" is no place the system knows{hint}; it was not taken. The system asked which \
                     city it is in"
                ));
                *self.state.place_rejections.entry(slot.to_string()).or_default() += 1;
                // "רמבם 12 בטרדיט" (ביתר עילית misheard): the street and number are kept, and only
                // the city is asked; asked for all of it again, a live caller had to repeat the street.
                if let Some(street) =
                    street_before_city(raw).filter(|_| self.business.response("street_city_unknown").is_some())
                {
                    notes.push(format!("the street \"{street}\" is kept: pass only the city the caller names"));
                    self.state
                        .doubt_confirm
                        .insert(slot.to_string(), vec![prompt("street_city_unknown", &[("street", street.clone())])]);
                    self.state.place_streets.insert(slot.to_string(), street);
                    rejected.push(slot.to_string());
                    return None;
                }
                let unknown = format!("{}_unknown", self.pipeline_ask(slot).unwrap_or_default());
                if self.business.response(&unknown).is_some() {
                    let heard = spoken.trim().to_string();
                    self.state.doubt_confirm.insert(slot.to_string(), vec![prompt(&unknown, &[("heard", heard)])]);
                }
                rejected.push(slot.to_string());
                return None;
            }
            // Taken as said after two misses with no desk: marked for the driver.
            Lookup::NoCity { .. } if precise || street_once => {
                notes.push(format!("{slot} \"{spoken}\" is taken as the caller said it, not checked"));
                SlotValue::Place {
                    address: Some(format!("{} (מקום לא מאומת: לתאם עם הנוסע)", spoken.trim())),
                    spoken: spoken.trim().to_string(),
                    customer_place: None,
                }
            }
            Lookup::NoCity { closest } => {
                let hint = if closest.is_empty() {
                    "ask which city".to_string()
                } else {
                    format!("closest localities: {}; if the caller meant one, confirm it", closest.join(", "))
                };
                notes.push(format!("{slot} \"{spoken}\" names no Israeli locality ({hint})"));
                value
            }
            // "בית דחה 45, אלעד": a house number makes it a street, not a landmark, and אלעד
            // has no such street. Most likely misheard: ask once more; the second time it is
            // kept (the list may lack a new street).
            // "רמבם" alone in ביתר עילית, which has הרמב"ן: offered too, not asked for a landmark's
            // address (a live caller was asked three times, then booked on a street that is not there).
            Lookup::NoStreet { city, heard, closest }
                if (said.chars().any(|c| c.is_ascii_digit()) || (precise && !closest.is_empty()))
                    && !self.business.is_informal_place(&heard)
                    && !give_up
                    && self.state.doubted_streets.insert(slot.to_string()) =>
            {
                let hint = if closest.is_empty() {
                    String::new()
                } else {
                    format!(
                        " (the closest streets there: {}; if one sounds like it, ask whether the caller meant it)",
                        closest.join(", ")
                    )
                };
                // Said back in so many words: not found there, did they mean the closest street, or
                // is it in another city. Asked for the street again, a live caller who had just
                // said it hung up. Insisting takes it as said; a third miss goes to a person.
                let number: String = said.chars().filter(char::is_ascii_digit).collect();
                let suggestion = closest.first().cloned();
                // As the caller said them: "ארנוביץ" (not the matching key "ארנוביצ"), "תל אביב"
                // (not "תל אביב - יפו").
                // The agent's own "street, city" keeps the comma the parsed value lost.
                let said_street: String = said
                    .split(',')
                    .next()
                    .unwrap_or(&said)
                    .chars()
                    .filter(|c| !c.is_ascii_digit())
                    .collect::<String>()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                let said_street = if said_street.is_empty() { heard.clone() } else { said_street };
                let said_city =
                    city_before.clone().unwrap_or_else(|| city.split(" - ").next().unwrap_or(&city).to_string());
                if let Some(s) = &suggestion {
                    if !self.state.offered_streets.contains(s) {
                        self.state.offered_streets.push(s.clone());
                    }
                }
                let prompts = match &suggestion {
                    Some(s) if self.business.response("street_not_found_suggest").is_some() => vec![prompt(
                        "street_not_found_suggest",
                        &[("heard", said_street.clone()), ("city", said_city.clone()), ("suggestion", s.clone())],
                    )],
                    _ if self.business.response("street_not_found").is_some() => {
                        vec![prompt("street_not_found", &[("heard", said_street.clone()), ("city", said_city.clone())])]
                    }
                    _ => vec![],
                };
                let meant = suggestion
                    .map(|s| format!("if they agree to {s}, pass \"{}, {city}\"; ", format!("{s} {number}").trim()))
                    .unwrap_or_default();
                let with_number =
                    if number.is_empty() { String::new() } else { format!(" and its house number {number}") };
                notes.push(format!(
                    "{slot}: {city} has no street \"{heard}\"; it was not taken. The system said so and asked what they \
                     meant: {meant}if they name another city, pass the street{with_number} with that city; if they insist \
                     on what they said, pass it again as it is: it will be taken{hint}"
                ));
                if !number.is_empty() {
                    self.state.doubted_numbers.insert(slot.to_string(), number.clone());
                }
                *self.state.place_rejections.entry(slot.to_string()).or_default() += 1;
                if !prompts.is_empty() {
                    self.state.doubt_confirm.insert(slot.to_string(), prompts);
                }
                self.state.place_cities.insert(slot.to_string(), city);
                rejected.push(slot.to_string());
                return None;
            }
            // "קניון הזהב, אלעד": neither a street nor a place on the list. Ask for its
            // address once; if the caller has none, it is taken as said, marked for the driver.
            Lookup::NoStreet { city, heard, closest }
                if !self.business.is_informal_place(&heard) && self.state.doubted_streets.insert(slot.to_string()) =>
            {
                let hint = if closest.is_empty() {
                    String::new()
                } else {
                    format!(
                        " (close names there: {}; if one sounds like it, ask whether the caller meant it)",
                        closest.join(", ")
                    )
                };
                notes.push(format!(
                    "{slot}: \"{heard}\" is not a street or a known place in {city}{hint}. Ask for its address{}; if \
                     the caller does not know it, pass the place again as they said it",
                    self.said_as("ask_place_address")
                ));
                self.state.place_cities.insert(slot.to_string(), city);
                rejected.push(slot.to_string());
                return None;
            }
            Lookup::NoStreet { city, .. } => {
                self.state.place_cities.remove(slot);
                // As the caller said it: "קניון הזהב, ירושלים".
                let name = spoken.trim().strip_suffix(city.as_str()).map(|p| p.trim().trim_end_matches(',').trim());
                let said = match name {
                    Some(name) if !name.is_empty() => format!("{name}, {city}"),
                    _ => spoken.clone(),
                };
                SlotValue::Place {
                    address: Some(format!("{said} (מקום לא מאומת: לתאם עם הנוסע)")),
                    spoken: said,
                    customer_place: None,
                }
            }
        })
    }

    /// An `ask_before_confirm` slot of the current task with no value, not asked by the engine
    /// or by the agent (its question is in the conversation), once every required one is in.
    fn unasked_before_confirm(&self) -> Option<String> {
        let run = self.state.run.as_ref()?;
        let pipeline = self.pipeline_of(run);
        if pipeline.slots.iter().any(|ps| ps.required && ps.default.is_none() && !run.slots.contains_key(&ps.slot)) {
            return None;
        }
        pipeline
            .slots
            .iter()
            .filter(|ps| ps.ask_before_confirm && !run.slots.contains_key(&ps.slot))
            .filter(|ps| !self.state.asked_before_confirm.contains(&ps.slot))
            .find(|ps| {
                let variants = ps.ask.as_ref().and_then(|a| self.business.response(a)).map(|r| r.variants.clone());
                !variants.unwrap_or_default().iter().any(|v| {
                    let v = crate::text::normalize(v);
                    self.state
                        .history
                        .iter()
                        .any(|t| t.speaker == Speaker::Agent && crate::text::normalize(&t.text).contains(&v))
                })
            })
            .map(|ps| ps.slot.clone())
    }

    /// Read the task back for a yes/no, or ask for what is still missing.
    fn read_back(&mut self, out: &mut Out, ask_if_missing: bool) {
        let Some(run) = &self.state.run else { return };
        let pipeline = self.pipeline_of(run).clone();
        self.fill_defaults(&pipeline);
        let Some(run) = &self.state.run else { return };
        if let Some(missing) = pipeline.slots.iter().find(|ps| ps.required && !run.slots.contains_key(&ps.slot)) {
            if ask_if_missing {
                let slot = missing.slot.clone();
                self.ask(out, &slot, false);
            }
            return;
        }
        match &pipeline.confirm {
            Some(confirm) => {
                if let Some(run) = &mut self.state.run {
                    run.step = Step::AwaitingConfirmation;
                    run.confirmed = false;
                }
                let ctx = self.render_ctx(None);
                // A name taken from an earlier ride was never said in this call: the read-back
                // says it ("מיופטף", heard in an earlier call, went out unsaid).
                let remembered = self.state.run.as_ref().is_some_and(|r| {
                    pipeline.slots.iter().any(|ps| {
                        ps.from_customer.as_deref() == Some("name")
                            && r.slots.get(&ps.slot).is_some_and(|s| s.provenance == Provenance::Customer)
                    })
                });
                let with_name = format!("{}_with_name", confirm.response);
                let response = if remembered && self.business.response(&with_name).is_some() {
                    with_name
                } else {
                    confirm.response.clone()
                };
                self.say(out, &response, ctx, true);
            }
            // Nothing to confirm: go straight to the action.
            None => self.advance(out, false),
        }
    }

    /// The caller's own words close the call: a goodbye, "that's all", or a plain "no,
    /// thanks" after "anything else?". An LLM's reading of garbled speech is not enough.
    /// A goodbye of a word or two while a booking is being collected and a question waits for
    /// its answer: more likely a word misheard than a caller leaving mid-sentence. Once a call.
    fn goodbye_mid_task(&self, transcript: &str) -> bool {
        let Some(run) = &self.state.run else { return false };
        let words = self.business.fillers.strip(&crate::text::normalize(transcript)).split_whitespace().count();
        !self.state.goodbye_doubted
            && !self.offered_more
            && matches!(run.step, Step::Collecting { .. })
            && !run.slots.is_empty()
            && self.last_agent_asked()
            && words <= 2
    }

    /// What a second hearing of the caller's last words listens for, when the stream's
    /// transcript is worth hearing again: the question and the names its answer should be one
    /// of. A street of the city asked (unless the stream already wrote one of its streets), a
    /// city (unless it named a town), or a number of passengers (unless it has a number: "שתיים"
    /// was heard "ביי." and the call hung up).
    pub fn second_hearing_question(&self, transcript: &str) -> Option<(String, Vec<String>)> {
        let g = self.gazetteer.as_ref()?;
        if let Some(city) = self.street_focus() {
            let street_found = matches!(
                g.resolve_within(transcript, &city),
                Some(crate::gazetteer::Lookup::Found(a)) if a.street.is_some()
            );
            return (!street_found)
                .then(|| (format!("which street in {city}"), g.street_candidates(&city, transcript, 700, 150)));
        }
        if self.awaiting_city() && g.towns_named(transcript).is_empty() {
            return Some(("which town or city, from where and to where".into(), g.town_names(20)));
        }
        if self.state.last_asks.iter().any(|s| s == "passengers") && !has_a_number(transcript) {
            let numbers = PASSENGER_WORDS.iter().map(|w| w.to_string()).collect();
            return Some(("how many passengers (a number)".into(), numbers));
        }
        None
    }

    /// The second hearing as the agent gets it: a street it named without the house number the
    /// stream heard gets that number ("עריף שתים עשרה." heard again as "ראב\"ד" went out as
    /// ראב"ד with no number).
    pub fn with_stream_number(&self, transcript: &str, second: &str) -> String {
        if self.street_focus().is_none() || second.chars().any(|c| c.is_ascii_digit()) {
            return second.to_string();
        }
        let digits = crate::hebrew::with_digits(&crate::text::normalize(transcript));
        match digits.split_whitespace().find(|w| w.chars().all(|c| c.is_ascii_digit())) {
            Some(number) => format!("{second} {number}"),
            None => second.to_string(),
        }
    }

    /// What went wrong in the call so far, for its health record: a booking with details given
    /// and never sent, the same question twice in a row.
    pub fn health(&self) -> Vec<&'static str> {
        let mut problems = Vec::new();
        let sent = self.state.completed.iter().any(|c| c.outcome == "success");
        if !sent && self.state.run.as_ref().is_some_and(|r| !r.slots.is_empty()) {
            problems.push("booking_left_unfinished");
        }
        let questions: Vec<String> = self
            .state
            .history
            .iter()
            .filter(|t| t.speaker == Speaker::Agent && t.text.trim_end().ends_with('?'))
            .map(|t| crate::text::normalize(&t.text))
            .collect();
        if questions.windows(2).any(|w| w[0] == w[1]) {
            problems.push("same_question_twice");
        }
        problems
    }

    /// The agent's last words before the caller's were a question.
    fn last_agent_asked(&self) -> bool {
        self.state
            .history
            .iter()
            .rev()
            .find(|t| t.speaker == Speaker::Agent)
            .is_some_and(|t| t.text.trim_end().ends_with('?'))
    }

    fn caller_said_goodbye(&self, transcript: &str) -> bool {
        let norm = crate::text::normalize(transcript);
        let b = &self.business;
        let goodbye = b.meta(MetaIntent::Goodbye).is_some_and(|m| {
            m.exact.matches_whole(&norm, &b.fillers) || m.phrases.find(&norm).is_some() || m.exact.find(&norm).is_some()
        });
        let declined = b.deny.find(&norm).is_some_and(|(_, (start, _))| start == 0);
        goodbye || declined
    }

    /// A final transcript, understood.
    pub fn on_utterance(&mut self, u: Understanding) -> Vec<Directive> {
        let mut out = Out::default();
        if self.state.phase != Phase::Active || u.noise {
            return Vec::new();
        }
        self.state.turns += 1;
        self.state.silence_reprompts = 0;
        self.state.waiting = false;
        self.state.remember(Speaker::Caller, &u.transcript);
        if u.frustrated && self.business.config.voice.deliveries.contains_key("calm") {
            self.state.delivery = Some("calm".into());
        }

        let has_content = !u.slots.is_empty() || u.affirm.is_some() || u.intent.is_some();
        let meta = match u.meta {
            Some(MetaIntent::Wait) if has_content => None,
            m => m,
        };
        if let Some(m) = meta {
            self.meta(&mut out, m);
            return self.finish(out);
        }
        self.understood(&mut out, u);
        self.finish(out)
    }

    fn understood(&mut self, out: &mut Out, u: Understanding) {
        let offered_more = std::mem::take(&mut self.offered_more);
        let mut progressed = false;

        // A single value being read back.
        if let Some(run) = &mut self.state.run {
            if let Step::ConfirmingSlot { slot } = run.step.clone() {
                if u.slot(&slot).is_none() {
                    match u.affirm {
                        Some(true) => {
                            if let Some(s) = run.slots.get_mut(&slot) {
                                s.confirmed = true;
                                s.provenance = Provenance::Confirmed;
                            }
                            run.step = Step::Collecting { awaiting: None };
                            self.state.fallback_level = 0;
                            self.advance(out, false);
                            return;
                        }
                        Some(false) => {
                            run.clear(&slot);
                            run.step = Step::Collecting { awaiting: Some(slot.clone()) };
                            self.state.fallback_level = 0;
                            self.ask(out, &slot, false);
                            return;
                        }
                        None => {}
                    }
                }
                // Another doubtful value instead of yes or no is usually the recognizer
                // mishearing a correction. Reading that back too ("בנלחב, נכון?") loops, so
                // ask the question again instead, on the fallback ladder.
                let doubtful = u.slot(&slot).is_some_and(|f| {
                    self.business.config.slots.get(&slot).is_some_and(|cfg| f.confidence < cfg.confirm_below)
                });
                if doubtful {
                    run.clear(&slot);
                    run.step = Step::Collecting { awaiting: Some(slot.clone()) };
                    return self.fallback(out);
                }
            }
        }

        // The full read-back.
        if let Some(run) = &self.state.run {
            if run.step == Step::AwaitingConfirmation {
                let pipeline = self.pipeline_of(run);
                let corrections = u.slots.iter().any(|s| pipeline.slots.iter().any(|p| p.slot == s.slot));
                if !corrections {
                    match u.affirm {
                        Some(true) => {
                            if let Some(run) = &mut self.state.run {
                                run.confirmed = true;
                                for s in run.slots.values_mut() {
                                    s.confirmed = true;
                                }
                            }
                            self.state.fallback_level = 0;
                            self.advance(out, false);
                            return;
                        }
                        Some(false) => {
                            let ask_change = pipeline.confirm.as_ref().map(|c| c.ask_change.clone());
                            let nothing_to_change = pipeline.slots.is_empty();
                            self.state.fallback_level = 0;
                            if let Some(r) = ask_change {
                                let ctx = self.render_ctx(None);
                                self.say(out, &r, ctx, true);
                            }
                            if nothing_to_change {
                                // A bare "are you sure?" answered no: the flow is simply dropped.
                                self.finish_run(out, "declined", None);
                            } else if let Some(run) = &mut self.state.run {
                                run.step = Step::Collecting { awaiting: None };
                            }
                            return;
                        }
                        None => {}
                    }
                }
            }
        }

        // Business intent: start, switch, or answer.
        if let Some(guess) = &u.intent {
            let Some(intent) = self.business.intent(&guess.id).cloned() else {
                tracing::warn!(intent = %guess.id, "understanding returned an unknown intent");
                return self.fallback(out);
            };
            let current = self.state.run.as_ref().map(|r| r.intent.clone());
            // Answer-only intents (FAQ) never disturb the flow, so they need less certainty
            // to interrupt it than an intent that would replace the active pipeline.
            let threshold =
                if intent.respond.is_some() { intent.switch_confidence.min(0.6) } else { intent.switch_confidence };
            let switch = match &current {
                None => true,
                Some(c) => *c != intent.id && guess.confidence >= threshold,
            };
            if switch {
                if intent.handoff {
                    return self.handoff(out, &format!("intent:{}", intent.id));
                }
                if let Some(r) = &intent.respond {
                    self.state.fallback_level = 0;
                    let ctx = self.render_ctx(None);
                    self.say(out, r, ctx, true);
                    if self.state.run.is_some() {
                        // Answer, then pick the pipeline back up where it was.
                        self.advance(out, false);
                    } else {
                        self.offer_more(out);
                    }
                    return;
                }
                if let Some(p) = &intent.pipeline {
                    if let Some(run) = self.state.run.take() {
                        if run.pipeline != *p && !run.slots.is_empty() {
                            self.state.suspended.push(run);
                        }
                    }
                    self.state.run = Some(self.new_run(p, &intent.id));
                    progressed = true;
                }
            }
        }

        // Values with no intent yet: infer the pipeline they belong to.
        if self.state.run.is_none() && !u.slots.is_empty() {
            if let Some((pipeline, intent)) = self.infer_pipeline(&u.slots) {
                self.state.run = Some(self.new_run(&pipeline, &intent));
                progressed = true;
            }
        }

        if self.state.run.is_none() {
            if offered_more && u.affirm == Some(false) {
                return self.goodbye(out);
            }
            if offered_more && u.affirm == Some(true) {
                let ctx = self.render_ctx(None);
                let greeting = self.business.config.greeting.clone();
                self.say(out, &greeting, ctx, true);
                return;
            }
            return self.fallback(out);
        }

        // Places as the agent's turns check them: a city alone for a place that needs its
        // street is noted and its street asked next ("איפה באלעד לאסוף?"), never taken as the
        // address. When the agent timed out a live call took "מודיעין עילית" and "בני ברק" as
        // they were and read back a ride with no street.
        let mut fills = Vec::with_capacity(u.slots.len());
        let (mut notes, mut rejected) = (Vec::new(), Vec::new());
        for fill in u.slots.iter().cloned() {
            // Only what would be taken: a guess the rules discard notes no city either.
            let accepted =
                self.business.config.slots.get(&fill.slot).is_some_and(|c| fill.confidence >= c.reject_below)
                    && self
                        .state
                        .run
                        .as_ref()
                        .is_some_and(|r| self.pipeline_of(r).slots.iter().any(|p| p.slot == fill.slot));
            let SlotValue::Place { spoken, .. } = &fill.value else {
                fills.push(fill);
                continue;
            };
            if !accepted {
                fills.push(fill);
                continue;
            }
            let raw = spoken.clone();
            match self.check_place(&fill.slot, fill.value.clone(), &raw, &mut notes, &mut rejected) {
                Some(value) => fills.push(SlotFill { value, ..fill }),
                None => progressed = true,
            }
        }
        let applied = self.apply_fills(&fills);
        if applied == 0 && !progressed {
            return self.fallback(out);
        }
        self.state.fallback_level = 0;
        if self.apply_rules(out) {
            return;
        }
        self.advance(out, applied > 0);
    }

    /// Store understood values for slots of the active pipeline. Returns how many were
    /// accepted.
    fn apply_fills(&mut self, fills: &[SlotFill]) -> usize {
        let Some(run) = &self.state.run else { return 0 };
        let pipeline = self.pipeline_of(run).clone();
        let mut accepted = 0;
        for fill in fills {
            if !pipeline.slots.iter().any(|p| p.slot == fill.slot) {
                continue;
            }
            let Some(cfg) = self.business.config.slots.get(&fill.slot) else { continue };
            if fill.confidence < cfg.reject_below {
                tracing::debug!(slot = %fill.slot, confidence = fill.confidence, "value rejected: too uncertain");
                continue;
            }
            let Some(run) = &mut self.state.run else { return accepted };
            if run.slots.get(&fill.slot).is_some_and(|s| s.value == fill.value) {
                accepted += 1;
                continue;
            }
            run.set(
                &fill.slot,
                SlotState {
                    value: fill.value.clone(),
                    confidence: fill.confidence,
                    provenance: fill.provenance,
                    confirmed: false,
                },
            );
            accepted += 1;
        }
        if accepted > 0 {
            if let Some(run) = &mut self.state.run {
                if matches!(run.step, Step::AwaitingConfirmation | Step::ConfirmingSlot { .. }) {
                    run.step = Step::Collecting { awaiting: None };
                }
            }
        }
        accepted
    }

    /// Whether a rule applies now: its task, not fired yet, its condition met.
    fn rule_hit(&self, rule: &crate::config::RuleConfig) -> bool {
        let Some(run) = &self.state.run else { return false };
        if rule.pipeline.as_ref().is_some_and(|p| *p != run.pipeline) || run.fired_rules.contains(&rule.id) {
            return false;
        }
        let value = run.slots.get(&rule.when.slot).map(|s| &s.value);
        match &rule.when.test {
            crate::config::ConditionTest::Gt(x) => value.and_then(SlotValue::as_f64).is_some_and(|v| v > *x),
            crate::config::ConditionTest::Lt(x) => value.and_then(SlotValue::as_f64).is_some_and(|v| v < *x),
            crate::config::ConditionTest::Eq(x) => value.is_some_and(|v| v.matches_json(x)),
            crate::config::ConditionTest::Present(p) => value.is_some() == *p,
        }
    }

    /// Whether a rule would take the turn over now (a handoff, a rejected value).
    fn rule_takes_over(&self) -> bool {
        self.business.config.rules.iter().any(|r| !matches!(r.then, RuleEffect::Set { .. }) && self.rule_hit(r))
    }

    /// Business rules. Returns true when a rule took over the turn.
    fn apply_rules(&mut self, out: &mut Out) -> bool {
        let rules = self.business.config.rules.clone();
        for rule in rules {
            if self.state.run.is_none() {
                return false;
            }
            if !self.rule_hit(&rule) {
                continue;
            }
            tracing::info!(rule = %rule.id, "business rule fired");
            if let Some(run) = &mut self.state.run {
                run.fired_rules.push(rule.id.clone());
            }
            match &rule.then {
                RuleEffect::Reject { response } => {
                    if let Some(run) = &mut self.state.run {
                        run.clear(&rule.when.slot);
                        run.step = Step::Collecting { awaiting: Some(rule.when.slot.clone()) };
                    }
                    let ctx = self.render_ctx(None);
                    self.say(out, response, ctx, true);
                    self.ask(out, &rule.when.slot, false);
                    return true;
                }
                RuleEffect::Handoff { response, reason } => {
                    if let Some(r) = response {
                        let ctx = self.render_ctx(None);
                        self.say(out, r, ctx, true);
                    }
                    self.handoff(out, &format!("rule:{reason}"));
                    return true;
                }
                RuleEffect::Set { slot, value } => {
                    if let Some(v) = default_value(&self.business, slot, value) {
                        if let Some(run) = &mut self.state.run {
                            run.set(
                                slot,
                                SlotState { value: v, confidence: 1.0, provenance: Provenance::Rule, confirmed: true },
                            );
                        }
                    }
                }
            }
        }
        false
    }

    /// Move the active pipeline forward: confirm a doubtful value, ask for the next
    /// missing one, read everything back, or run the action.
    fn advance(&mut self, out: &mut Out, acknowledge: bool) {
        let Some(run) = &self.state.run else { return };
        let pipeline = self.pipeline_of(run).clone();
        self.fill_defaults(&pipeline);

        let Some(run) = &self.state.run else { return };
        // A value too uncertain to use without asking.
        for ps in &pipeline.slots {
            let Some(s) = run.slots.get(&ps.slot) else { continue };
            let Some(cfg) = self.business.config.slots.get(&ps.slot) else { continue };
            if !s.confirmed && s.confidence < cfg.confirm_below {
                let spoken = s.value.spoken();
                if let Some(run) = &mut self.state.run {
                    run.step = Step::ConfirmingSlot { slot: ps.slot.clone() };
                }
                let mut ctx = self.render_ctx(None);
                ctx.extra.insert("value".into(), spoken);
                let r = self.business.config.confirm_slot.clone();
                self.say(out, &r, ctx, true);
                return;
            }
        }

        // The next missing required value.
        if let Some(missing) = pipeline.slots.iter().find(|ps| ps.required && !run.slots.contains_key(&ps.slot)) {
            let slot = missing.slot.clone();
            if let Some(run) = &mut self.state.run {
                run.step = Step::Collecting { awaiting: Some(slot.clone()) };
            }
            self.ask(out, &slot, acknowledge);
            return;
        }

        // Read-back.
        if let Some(confirm) = &pipeline.confirm {
            if !run.confirmed {
                if let Some(run) = &mut self.state.run {
                    run.step = Step::AwaitingConfirmation;
                }
                let ctx = self.render_ctx(None);
                self.say(out, &confirm.response, ctx, true);
                return;
            }
        }

        // The business action.
        if let Some(action_id) = &pipeline.action {
            let action = self.business.config.actions.get(action_id).cloned();
            if let Some(action) = &action {
                if let Some((slot, s)) =
                    run.slots.iter().find(|(_, s)| !s.confirmed && s.confidence < action.min_confidence)
                {
                    let (slot, spoken) = (slot.clone(), s.value.spoken());
                    if let Some(run) = &mut self.state.run {
                        run.step = Step::ConfirmingSlot { slot };
                    }
                    let mut ctx = self.render_ctx(None);
                    ctx.extra.insert("value".into(), spoken);
                    let r = self.business.config.confirm_slot.clone();
                    self.say(out, &r, ctx, true);
                    return;
                }
            }
            let run_id = self.state.next_action_run;
            self.state.next_action_run += 1;
            let input = self.action_input(run_id);
            // The same question already failed in this call (the price bot did not answer):
            // its failure now, not another wait for it.
            let changes = action.as_ref().is_some_and(|a| a.requires_confirmation);
            if !changes && self.state.failed_actions.contains(&failed_key(action_id, &input)) {
                tracing::info!(action = %action_id, "failed before in this call; not asked again");
                if let Some(r) = &pipeline.on_failure {
                    let ctx = self.render_ctx(None);
                    self.say(out, r, ctx, true);
                }
                self.finish_run(out, "failed", Some(json!({ "error": "failed before in this call" })));
                return;
            }
            if let Some(run) = &mut self.state.run {
                run.step = Step::Executing { action_run: run_id };
                run.attempts = 1;
            }
            // The action's "שנייה, אני בודק." covers its wait, unless the turn already said
            // something: "סגור, בודק לך את המחיר. שנייה, אני בודק." was said twice over.
            if let Some(filler) = pipeline.filler.as_ref().filter(|_| !out.has_speech()) {
                let ctx = self.render_ctx(None);
                if let Some(plan) = self.render_plan(filler, &ctx) {
                    out.speak(plan, false);
                    out.flush(true);
                }
            }
            out.push(Directive::RunAction { run_id, action: action_id.clone(), input });
            return;
        }

        if let Some(r) = &pipeline.on_complete {
            let ctx = self.render_ctx(None);
            self.say(out, r, ctx, true);
        }
        self.finish_run(out, "completed", None);
    }

    /// A new run, pre-filled with values the caller already gave earlier in this call for
    /// the same slots ("how much to the airport?" ... "ok, book it"). They are unconfirmed,
    /// so a read-back still covers them.
    fn new_run(&self, pipeline: &str, intent: &str) -> PipelineRun {
        let mut run = PipelineRun::new(pipeline, intent);
        let Some(config) = self.business.pipeline(pipeline) else { return run };
        for completed in self.state.completed.iter().rev().filter(|c| c.outcome != "cancelled").take(1) {
            for ps in &config.slots {
                if let Some(value) = completed.slots.get(&ps.slot) {
                    run.slots.entry(ps.slot.clone()).or_insert_with(|| SlotState {
                        value: value.clone(),
                        confidence: 0.85,
                        provenance: Provenance::Rules,
                        confirmed: false,
                    });
                }
            }
        }
        run
    }

    /// What a business action receives. `run_id` is the same on every attempt of one run, so
    /// the backend can tell a retry from a new request (see the idempotency key).
    fn action_input(&self, run_id: u64) -> serde_json::Value {
        let Some(run) = &self.state.run else { return json!({}) };
        let slots: serde_json::Map<String, serde_json::Value> =
            run.slots.iter().map(|(k, v)| (k.clone(), v.value.to_action_json())).collect();
        json!({
            "run_id": run_id,
            "pipeline": run.pipeline,
            "intent": run.intent,
            "slots": slots,
            "customer": self.state.customer,
            "caller_phone": self.state.caller_phone,
        })
    }

    /// Result of a [`Directive::RunAction`].
    pub fn on_action_result(
        &mut self,
        run_id: u64,
        result: Result<serde_json::Value, ActionFailure>,
    ) -> Vec<Directive> {
        let mut out = Out::default();
        let current = self.state.run.as_ref().map(|r| r.step.clone());
        if current != Some(Step::Executing { action_run: run_id }) {
            tracing::warn!(run_id, "action result for a run that is no longer active; ignored");
            return Vec::new();
        }
        let Some(run) = &self.state.run else { return Vec::new() };
        let pipeline = self.pipeline_of(run).clone();
        let action_id = pipeline.action.clone().unwrap_or_default();
        let max_attempts = self.business.config.actions.get(&action_id).map_or(1, |a| a.max_attempts);
        match result {
            Ok(value) => {
                // A result may name its own response (a price by car size, or there and back).
                let named = value
                    .get("response")
                    .and_then(|r| r.as_str())
                    .filter(|r| self.business.response(r).is_some())
                    .map(str::to_string);
                if let Some(r) = named.as_ref().or(pipeline.on_success.as_ref()) {
                    let ctx = self.render_ctx(Some(&value));
                    let ctx = RenderContext { result: Some(&value), ..ctx };
                    self.say(&mut out, r, ctx, true);
                }
                self.finish_run(&mut out, "success", Some(value));
            }
            Err(failure) => {
                tracing::warn!(action = %action_id, %failure, "business action failed");
                let attempts = run.attempts;
                // A retry carries the same run id, so a backend that honours the idempotency
                // key does the task once even when the first attempt went through.
                if attempts < max_attempts {
                    let input = self.action_input(run_id);
                    if let Some(run) = &mut self.state.run {
                        run.attempts += 1;
                    }
                    out.push(Directive::RunAction { run_id, action: action_id, input });
                    return self.finish(out);
                }
                if failure.outcome_unknown {
                    // The ride may be on its way: never "no driver available" for it. A person
                    // checks, with the details in the handoff summary.
                    let result = json!({ "error": failure.error, "outcome_unknown": true, "run_id": run_id });
                    if self.has_desk() {
                        // "A dispatcher will make sure it went through": only when one can.
                        if let Some(r) = &pipeline.on_unknown {
                            let ctx = self.render_ctx(None);
                            self.say(&mut out, r, ctx, true);
                        }
                        self.handoff(&mut out, "action_outcome_unknown");
                        self.close_run("unknown", Some(result));
                    } else {
                        self.finish_run(&mut out, "unknown", Some(result));
                    }
                    return self.finish(out);
                }
                let error = failure.error;
                // Only a task that changes something counts toward handing the call over: a
                // price bot that did not answer is no reason to.
                let changes = self.business.config.actions.get(&action_id).is_some_and(|a| a.requires_confirmation);
                if changes {
                    self.state.action_failures += 1;
                } else {
                    let key = failed_key(&action_id, &self.action_input(run_id));
                    if !self.state.failed_actions.contains(&key) {
                        self.state.failed_actions.push(key);
                    }
                }
                if let Some(r) = &pipeline.on_failure {
                    let ctx = self.render_ctx(None);
                    self.say(&mut out, r, ctx, true);
                }
                if self.state.action_failures >= self.business.config.handoff.after_action_failures {
                    self.handoff(&mut out, "action_failed");
                } else {
                    self.finish_run(&mut out, "failed", Some(json!({ "error": error })));
                }
            }
        }
        self.finish(out)
    }

    /// Render a response outside the conversation flow (e.g. a "thinking" filler). Not
    /// remembered as the last thing said.
    pub fn render_response(&mut self, response_id: &str) -> Option<SpeechPlan> {
        let ctx = self.render_ctx(None);
        self.render_plan(response_id, &ctx)
    }

    /// Hand the call to a human for a reason outside the conversation (speech recognition
    /// unavailable, internal error). Falls back to the unavailable message and a hangup.
    pub fn force_handoff(&mut self, reason: &str) -> Vec<Directive> {
        let mut out = Out::default();
        if self.state.phase != Phase::Active {
            return Vec::new();
        }
        self.handoff(&mut out, reason);
        if self.state.phase == Phase::Active {
            self.state.phase = Phase::Ending;
            out.push(Directive::Hangup);
        }
        self.finish(out)
    }

    /// The caller said something that could not be made out (line noise, or words the
    /// recognizer turned into nothing) and then nothing more: the line is noisy, said so,
    /// and the question again ("סליחה, יש קצת רעש בקו. כמה נוסעים?"), so the caller knows
    /// why it is asked twice. Said once per turn: after that, the silence reprompt waits.
    pub fn on_unheard(&mut self) -> Vec<Directive> {
        // Before any task, the question would be the greeting again: the silence reprompt waits.
        if self.state.run.is_none() {
            return Vec::new();
        }
        self.on_noise(false)
    }

    /// Noise on the line. `interrupted`: it cut the agent off, and what was being said is said
    /// again (all of it: a read-back cut in the middle left details unheard). Otherwise the
    /// question again. The first time in a turn, after "the line is noisy"; an empty result
    /// means nothing is said (the silence reprompt is the backstop).
    pub fn on_noise(&mut self, interrupted: bool) -> Vec<Directive> {
        let mut out = Out::default();
        if self.state.phase != Phase::Active
            || matches!(self.state.run.as_ref().map(|r| &r.step), Some(Step::Executing { .. }))
        {
            return Vec::new();
        }
        let apologize =
            self.state.noise_apology_turn != Some(self.state.turns) && self.business.response("noisy_line").is_some();
        let again = if interrupted { self.state.last_plan.clone() } else { self.last_question() };
        match again {
            Some(plan) => {
                if !interrupted && !apologize {
                    return Vec::new();
                }
                if apologize {
                    self.state.noise_apology_turn = Some(self.state.turns);
                    let ctx = self.render_ctx(None);
                    self.say(&mut out, "noisy_line", ctx, false);
                }
                out.speak(plan, false);
            }
            // Nothing to ask again: "say that again?", which gives its own reason.
            None if !interrupted && apologize => {
                self.state.noise_apology_turn = Some(self.state.turns);
                if let Some(r) = self.business.config.fallback.ladder.first().cloned() {
                    let ctx = self.render_ctx(None);
                    self.say(&mut out, &r, ctx, false);
                }
            }
            None => return Vec::new(),
        }
        self.finish(out)
    }

    /// "הלו?" while the caller waits: "כן, אני פה." and the question again.
    pub fn on_hello(&mut self) -> Vec<Directive> {
        let mut out = Out::default();
        if self.state.phase != Phase::Active
            || matches!(self.state.run.as_ref().map(|r| &r.step), Some(Step::Executing { .. }))
            || self.business.response("i_hear_you").is_none()
        {
            return self.on_noise(false);
        }
        let ctx = self.render_ctx(None);
        self.say(&mut out, "i_hear_you", ctx, false);
        if let Some(q) = self.last_question() {
            out.speak(q, false);
        }
        self.finish(out)
    }

    /// Words that only check the line is still there ("הלו").
    pub fn is_hello(&self, text: &str) -> bool {
        let norm = crate::text::normalize(text);
        !self.business.hello.is_empty()
            && self.business.hello.find(&norm).is_some()
            && self.business.fillers.strip(&self.business.hello.strip(&norm)).trim().is_empty()
    }

    /// Nothing but filler words ("תודה רבה", "טוב").
    fn only_fillers(&self, text: &str) -> bool {
        let norm = crate::text::normalize(text);
        !norm.trim().is_empty() && self.business.fillers.strip(&norm).trim().is_empty()
    }

    /// "כן", "אהה", "בסדר" said while the details are read back: the caller listening, not
    /// the answer. It neither stops the read-back nor sends the task before it was heard.
    pub fn is_backchannel(&self, text: &str) -> bool {
        self.context().awaiting_confirmation && self.is_listening_sound(text)
    }

    /// "כן", "אהה", "mm": the caller listening, whatever the call is waiting for. The same
    /// test as [`Engine::is_backchannel`] without the read-back; used to tell an interruption
    /// from a caller who is only showing they follow.
    pub fn is_listening_sound(&self, text: &str) -> bool {
        let norm = crate::text::normalize(text);
        let rest = self.business.fillers.strip(&self.business.affirm.strip(&norm));
        !norm.trim().is_empty() && crate::text::tokens(&rest).iter().all(|t| crate::understanding::is_hesitation(t))
    }

    /// A short answer the call is waiting for, in words recognizers also invent on noise
    /// ("טוב", "תודה" to "משהו נוסף?", to the read-back, to the question for the driver):
    /// taken as an answer, not dropped as noise.
    pub fn takes_short_answer(&self, text: &str) -> bool {
        let driver_question = self.state.run.as_ref().is_some_and(|r| {
            self.pipeline_of(r).slots.iter().any(|ps| ps.ask_before_confirm && self.state.last_asks.contains(&ps.slot))
        });
        let expects = self.offered_more || self.context().awaiting_confirmation || driver_question;
        let norm = crate::text::normalize(text);
        expects
            && !self.is_hello(text)
            && crate::text::tokens(&norm).iter().any(|t| !crate::understanding::is_hesitation(t))
    }

    /// "לא", "אין, תודה" to the optional question asked before the read-back ("הערה לנהג?"):
    /// nothing for the driver, and the details are read back. Without the agent: a live call
    /// answered this "לא" with "אפשר להמשיך?".
    pub fn on_no_to_optional(&mut self, text: &str) -> Option<Vec<Directive>> {
        let run = self.state.run.as_ref()?;
        if self.state.phase != Phase::Active || !matches!(run.step, Step::Collecting { .. }) {
            return None;
        }
        let pipeline = self.pipeline_of(run);
        let slot = pipeline
            .slots
            .iter()
            .find(|ps| {
                ps.ask_before_confirm && self.state.last_asks.contains(&ps.slot) && !run.slots.contains_key(&ps.slot)
            })?
            .slot
            .clone();
        let norm = crate::text::normalize(text);
        let said_no = self.business.deny.find(&norm).is_some() || self.business.nothing.find(&norm).is_some();
        let rest = self.business.fillers.strip(&self.business.nothing.strip(&self.business.deny.strip(&norm)));
        if !said_no || !rest.trim().is_empty() {
            return None;
        }
        tracing::info!(text, %slot, "nothing for the optional question; reading back");
        self.state.turns += 1;
        self.state.silence_reprompts = 0;
        self.state.remember(Speaker::Caller, text);
        self.state.asked_before_confirm.insert(slot);
        self.state.last_asks.clear();
        let mut out = Out::default();
        self.read_back(&mut out, true);
        Some(self.finish(out))
    }

    /// "תודה", "טוב תודה" to "משהו נוסף?": the call ends with the goodbye.
    pub fn on_closing(&mut self, text: &str) -> Option<Vec<Directive>> {
        let norm = crate::text::normalize(text);
        if self.state.phase != Phase::Active
            || !self.offered_more
            || !self.only_fillers(text)
            || self.is_hello(text)
            || crate::text::tokens(&norm).iter().all(|t| crate::understanding::is_hesitation(t))
        {
            return None;
        }
        self.offered_more = false;
        self.state.turns += 1;
        self.state.remember(Speaker::Caller, text);
        let mut out = Out::default();
        self.goodbye(&mut out);
        Some(self.finish(out))
    }

    /// How long a silence waits before its reprompt: longer after "רגע", and between the
    /// patient reprompts.
    pub fn silence_after_ms(&self) -> u64 {
        let s = &self.business.config.silence;
        if self.state.waiting || self.state.silence_reprompts > s.max_reprompts {
            s.patient_after_ms
        } else {
            s.reprompt_after_ms
        }
    }

    /// A plan without the "the line is noisy" said at its start.
    fn without_noise_apology(&self, mut plan: SpeechPlan) -> SpeechPlan {
        let apologies: Vec<String> = self
            .business
            .response("noisy_line")
            .map(|r| r.variants.iter().map(|v| crate::text::normalize(v)).collect())
            .unwrap_or_default();
        plan.segments.retain(|s| !apologies.contains(&crate::text::normalize(&s.text)));
        plan
    }

    /// The words of the last question said ("... לשלוח?"), to tell a read-back said again
    /// word for word from one with something changed in it.
    pub fn last_question_text(&self) -> Option<String> {
        let plan = self.state.last_plan.as_ref()?;
        plan.segments.iter().rev().find(|s| s.text.trim_end().ends_with('?')).map(|s| s.text.clone())
    }

    /// The last question alone ("לאן נוסעים?", not the "סגור." said before it).
    fn last_question(&self) -> Option<SpeechPlan> {
        self.state.last_plan.as_ref().and_then(|p| {
            let last = p.segments.last().filter(|s| s.text.trim_end().ends_with('?'))?;
            let text = last.text.trim_end();
            let start = text[..text.len() - 1].rfind(['.', '!', '?']).map_or(0, |i| i + 1);
            Some(match text[start..].trim() {
                tail if start > 0 => SpeechPlan::free(tail, &last.delivery, p.gain_db),
                _ => SpeechPlan { segments: vec![last.clone()], gain_db: p.gain_db },
            })
        })
    }

    /// The last question once more, alone; with no question pending, the business's first
    /// fallback.
    fn question_again(&mut self, out: &mut Out) {
        match self.last_question() {
            Some(question) => out.speak(question, false),
            None => {
                if let Some(r) = self.business.config.fallback.ladder.first().cloned() {
                    let ctx = self.render_ctx(None);
                    self.say(out, &r, ctx, false);
                }
            }
        }
    }

    /// The caller said nothing for the configured silence.
    pub fn on_silence(&mut self) -> Vec<Directive> {
        let mut out = Out::default();
        if self.state.phase != Phase::Active
            || matches!(self.state.run.as_ref().map(|r| &r.step), Some(Step::Executing { .. }))
        {
            return Vec::new();
        }
        self.state.waiting = false;
        // The ride is sent and "משהו נוסף?" got no answer: the caller is done, not gone. A live
        // call heard "הלו? אני פה. אפשר לעזור במשהו נוסף?" here; a goodbye ends it.
        if self.offered_more
            && self.state.run.is_none()
            && self.state.completed.last().is_some_and(|c| c.outcome == "success")
        {
            self.goodbye(&mut out);
            return self.finish(out);
        }
        self.state.silence_reprompts += 1;
        let silence = self.business.config.silence.clone();
        // Details already given: a few patient reprompts more before hanging up on them.
        let in_task = self.state.run.as_ref().is_some_and(|r| !r.slots.is_empty());
        let patient = if in_task && silence.patient_response.is_some() { silence.patient_reprompts } else { 0 };
        if self.state.silence_reprompts > silence.max_reprompts + patient {
            self.goodbye(&mut out);
            return self.finish(out);
        }
        if self.state.silence_reprompts > silence.max_reprompts {
            if let Some(r) = &silence.patient_response {
                let ctx = self.render_ctx(None);
                self.say(&mut out, r, ctx, false);
                return self.finish(out);
            }
        }
        // What was said last, without its "סליחה, יש קצת רעש בקו.": a live call heard "הלו,
        // שומעים אותי? סליחה, יש קצת רעש בקו. צריך מונית?".
        // Only its question: a live call's reprompt said "רגע, מעביר למוקדן שיתקן את ההזמנה.
        // אחלה. אז שבעה נוסעים ... לשלוח?" again, twice.
        // And no question, nothing again: "הלו? אני פה. רגע, בודק." was said three times.
        let last = self.state.last_plan.clone().map(|p| self.without_noise_apology(p)).map(|mut p| {
            match p.segments.iter().rposition(|s| s.text.trim_end().ends_with('?')) {
                Some(i) => p.segments = vec![p.segments[i].clone()],
                None => p.segments.clear(),
            }
            p
        });
        if let Some(r) = &silence.response {
            let ctx = self.render_ctx(None);
            self.say(&mut out, r, ctx, false);
        }
        if let Some(last) = last.filter(|p| !p.is_empty()) {
            out.speak(last, false);
        }
        self.finish(out)
    }

    // -----------------------------------------------------------------------------------

    fn meta(&mut self, out: &mut Out, m: MetaIntent) {
        let meta_response = self.business.meta(m).and_then(|c| c.response.clone());
        let has_slow = self.business.config.voice.deliveries.contains_key("slow");
        match m {
            MetaIntent::RepeatLast
            | MetaIntent::DidNotUnderstand
            | MetaIntent::SpeakSlower
            | MetaIntent::SpeakLouder => {
                if m == MetaIntent::SpeakSlower && has_slow {
                    self.state.delivery = Some("slow".into());
                }
                if m == MetaIntent::SpeakLouder {
                    self.state.gain_db = (self.state.gain_db + 4.0).min(8.0);
                }
                if let Some(r) = meta_response {
                    let ctx = self.render_ctx(None);
                    self.say(out, &r, ctx, false);
                }
                let mut last = match self.state.last_plan.clone() {
                    Some(p) => p,
                    None => {
                        let ctx = self.render_ctx(None);
                        let g = self.business.config.greeting.clone();
                        self.render_plan(&g, &ctx).unwrap_or_else(SpeechPlan::empty)
                    }
                };
                let slower = matches!(m, MetaIntent::DidNotUnderstand | MetaIntent::SpeakSlower) && has_slow;
                for seg in &mut last.segments {
                    if slower {
                        seg.delivery = "slow".into();
                    } else if let Some(d) = &self.state.delivery {
                        seg.delivery = d.clone();
                    }
                }
                last.gain_db = self.state.gain_db;
                out.speak(last, true);
            }
            MetaIntent::CancelCurrentFlow => {
                if let Some(run) = self.state.run.take() {
                    self.state.completed.push(CompletedRun {
                        pipeline: run.pipeline.clone(),
                        outcome: "cancelled".into(),
                        slots: run.slots.iter().map(|(k, v)| (k.clone(), v.value.clone())).collect(),
                        result: None,
                    });
                }
                self.state.suspended.clear();
                if let Some(r) = meta_response {
                    let ctx = self.render_ctx(None);
                    self.say(out, &r, ctx, true);
                }
                self.offered_more = true;
            }
            MetaIntent::GoBack => {
                let undone = self.state.run.as_mut().and_then(PipelineRun::undo);
                match undone {
                    Some(slot) => {
                        if let Some(run) = &mut self.state.run {
                            run.step = Step::Collecting { awaiting: Some(slot.clone()) };
                        }
                        if let Some(r) = meta_response {
                            let ctx = self.render_ctx(None);
                            self.say(out, &r, ctx, false);
                        }
                        self.ask(out, &slot, false);
                    }
                    None => {
                        if let Some(last) = self.state.last_plan.clone() {
                            out.speak(last, true);
                        }
                    }
                }
            }
            MetaIntent::TransferHuman => self.handoff(out, "caller_requested"),
            MetaIntent::Goodbye => self.goodbye(out),
            // "רגע": "בטח, אני מחכה." and a longer wait before the silence reprompt.
            MetaIntent::Wait => {
                if let Some(r) = meta_response {
                    let ctx = self.render_ctx(None);
                    self.say(out, &r, ctx, false);
                }
                self.state.waiting = true;
            }
        }
    }

    fn fallback(&mut self, out: &mut Out) {
        self.state.fallback_level += 1;
        let ladder = self.business.config.fallback.ladder.clone();
        let level = self.state.fallback_level as usize;
        if level > ladder.len() {
            self.handoff(out, "not_understood");
            return;
        }
        let in_pipeline = self.state.run.as_ref().is_some_and(|r| !matches!(r.step, Step::Executing { .. }));
        if in_pipeline {
            // Inside a flow: a short "didn't catch that" and the pending question again,
            // so the caller is never pulled out of what they were doing.
            let ctx = self.render_ctx(None);
            self.say(out, &ladder[0], ctx, false);
            self.advance(out, false);
        } else {
            let ctx = self.render_ctx(None);
            self.say(out, &ladder[level - 1], ctx, true);
        }
    }

    fn handoff(&mut self, out: &mut Out, reason: &str) {
        let handoff = self.business.config.handoff.clone();
        if self.has_desk() {
            let ctx = self.render_ctx(None);
            self.say(out, &handoff.response, ctx, true);
            self.state.phase = Phase::HandingOff;
            let summary = self.summary(reason);
            out.push(Directive::Handoff { summary });
            return;
        }
        if reason == "not_understood" && self.state.fallback_restarts == 0 {
            if let Some(restart) = self.business.config.fallback.restart.clone() {
                self.state.fallback_restarts += 1;
                self.state.fallback_level = 0;
                let ctx = self.render_ctx(None);
                self.say(out, &restart, ctx, true);
                return;
            }
        }
        let ctx = self.render_ctx(None);
        self.say(out, &handoff.unavailable_response, ctx, true);
        if reason == "not_understood" || reason == "action_failed" {
            self.state.phase = Phase::Ending;
            out.push(Directive::Hangup);
        } else if self.state.run.is_some() {
            self.advance(out, false);
        }
    }

    fn goodbye(&mut self, out: &mut Out) {
        let ctx = self.render_ctx(None);
        let goodbye = self
            .business
            .meta(MetaIntent::Goodbye)
            .and_then(|m| m.response.clone())
            .unwrap_or_else(|| self.business.config.goodbye.clone());
        self.say(out, &goodbye, ctx, true);
        self.state.phase = Phase::Ending;
        out.push(Directive::Hangup);
    }

    fn offer_more(&mut self, out: &mut Out) {
        let ctx = self.render_ctx(None);
        let r = self.business.config.anything_else.clone();
        self.say(out, &r, ctx, true);
        self.offered_more = true;
    }

    /// Record the current run as done, saying nothing (the call is being handed off).
    fn close_run(&mut self, outcome: &str, result: Option<serde_json::Value>) {
        let Some(run) = self.state.run.take() else { return };
        self.state.completed.push(CompletedRun {
            pipeline: run.pipeline.clone(),
            outcome: outcome.to_string(),
            slots: run.slots.iter().map(|(k, v)| (k.clone(), v.value.clone())).collect(),
            result,
        });
    }

    fn finish_run(&mut self, out: &mut Out, outcome: &str, result: Option<serde_json::Value>) {
        let Some(run) = self.state.run.take() else { return };
        let after = self.pipeline_of(&run).after;
        let offer = self.pipeline_of(&run).offer.clone();
        self.state.completed.push(CompletedRun {
            pipeline: run.pipeline.clone(),
            outcome: outcome.to_string(),
            slots: run.slots.iter().map(|(k, v)| (k.clone(), v.value.clone())).collect(),
            result,
        });
        if let Some(resumed) = self.state.suspended.pop() {
            self.state.run = Some(resumed);
            self.advance(out, false);
            return;
        }
        match (after, offer) {
            (AfterPipeline::Continue, Some(offer)) if self.business.response(&offer).is_some() => {
                let ctx = self.render_ctx(None);
                self.say(out, &offer, ctx, true);
                self.offered_more = true;
            }
            (AfterPipeline::Continue, _) => self.offer_more(out),
            (AfterPipeline::End, _) => self.goodbye(out),
        }
    }

    /// The question for what is still missing of a place: its house number when the street is
    /// known, its street when the city is, its city when the street is.
    fn specific_ask(&self, slot: &str) -> Option<String> {
        let ask = self.pipeline_ask(slot)?;
        let street = format!("{ask}_street");
        let city = format!("{ask}_city");
        let number = format!("{ask}_number");
        if self.state.place_numbers.contains_key(slot) && self.business.response(&number).is_some() {
            Some(number)
        } else if self.state.place_cities.contains_key(slot) && self.business.response(&street).is_some() {
            Some(street)
        } else if self.state.place_streets.contains_key(slot) && self.business.response(&city).is_some() {
            Some(city)
        } else {
            None
        }
    }

    /// A recorded phrase the agent chose. One that asks for a place in general ("לאן צריך
    /// להגיע?") after part of it was given (the city בני ברק) asks for what is missing
    /// instead ("לאן בבני ברק?"): a live caller was asked for a destination already given.
    pub fn render_phrase(&mut self, id: &str) -> Option<SpeechPlan> {
        let specific = self.slot_asked_by(id).and_then(|slot| self.specific_ask(&slot).map(|r| (slot, r)));
        match specific {
            Some((slot, response)) if response != id => {
                tracing::info!(phrase = id, instead = %response, "the phrase asks for what is already known; asking for the rest");
                let mut ctx = self.render_ctx(None);
                if let Some(city) = self.state.place_cities.get(&slot) {
                    ctx.extra.insert("city".into(), city.clone());
                }
                self.render_plan(&response, &ctx)
            }
            _ => match self.said_before(id).then(|| self.business.response(id).and_then(|r| r.again.clone())).flatten()
            {
                Some(again) => {
                    tracing::info!(phrase = id, instead = %again, "the phrase was said already in this call");
                    self.render_response(&again).or_else(|| self.render_response(id))
                }
                None => self.render_response(id),
            },
        }
    }

    /// One of the response's sentences was said earlier in this call.
    fn said_before(&self, id: &str) -> bool {
        let Some(r) = self.business.response(id) else { return false };
        let norm: Vec<String> = r.variants.iter().map(|v| crate::text::normalize(v)).collect();
        self.state.history.iter().any(|t| {
            t.speaker == Speaker::Agent && {
                let said = crate::text::normalize(&t.text);
                norm.iter().any(|v| !v.is_empty() && said.contains(v.as_str()))
            }
        })
    }

    fn ask(&mut self, out: &mut Out, slot: &str, acknowledge: bool) {
        let Some(run) = &self.state.run else { return };
        let pipeline = self.pipeline_of(run);
        let Some(mut ask) = pipeline.slots.iter().find(|p| p.slot == slot).and_then(|p| p.ask.clone()) else { return };
        // The city is known: only the street is missing ("לאיזה רחוב?", not "לאן?"); the street
        // is known: only its house number, or its city ("מאיזו עיר לאסוף?").
        if let Some(specific) = self.specific_ask(slot) {
            ask = specific;
        }
        // "לאן בבני ברק?": the city as understood, so a misheard one is heard and corrected.
        let mut ctx = self.render_ctx(None);
        if let Some(city) = self.state.place_cities.get(slot) {
            ctx.extra.insert("city".into(), city.clone());
        }
        // Asked last turn and not understood: the same words again sounded like a stuck machine
        // ("לאן בבני ברק?" twice). Its "again" says so and asks in other words.
        let last_said = self
            .state
            .history
            .iter()
            .rev()
            .find(|t| t.speaker == Speaker::Agent)
            .map(|t| crate::text::normalize(&t.text));
        // Only a question with other words for it is rendered here (rendering picks a variant).
        let has_again = self.business.response(&ask).is_some_and(|r| r.again.is_some());
        let same_again = has_again
            && self.state.last_asks.iter().any(|a| a == slot)
            && self.render_plan(&ask, &ctx).is_some_and(|p| {
                let words = crate::text::normalize(&p.text());
                last_said.as_ref().is_some_and(|l| !words.is_empty() && l.contains(words.as_str()))
            });
        if same_again {
            if let Some(again) = self.business.response(&ask).and_then(|r| r.again.clone()) {
                tracing::info!(slot, asked = %ask, instead = %again, "asked again: in other words");
                ask = again;
            }
        }
        let has_prefix = self.business.response(&ask).is_some_and(|r| r.prefix.is_some());
        if acknowledge && !has_prefix {
            if let Some(ack) = self.business.config.acknowledgement.clone() {
                let ctx = self.render_ctx(None);
                self.say(out, &ack, ctx, true);
            }
        }
        self.say(out, &ask, ctx, true);
        self.note_asked(&[slot.to_string()]);
    }

    fn infer_pipeline(&self, fills: &[SlotFill]) -> Option<(String, String)> {
        let mut candidates = self.business.config.intents.iter().filter_map(|i| {
            let p = i.pipeline.as_ref()?;
            let pipeline = self.business.pipeline(p)?;
            fills.iter().all(|f| pipeline.slots.iter().any(|s| s.slot == f.slot)).then(|| (p.clone(), i.id.clone()))
        });
        let first = candidates.next()?;
        candidates.next().is_none().then_some(first)
    }

    fn summary(&self, reason: &str) -> HandoffSummary {
        let run = self.state.run.as_ref().or(self.state.suspended.last());
        let mut slots = Vec::new();
        if let Some(run) = run {
            let pipeline = self.pipeline_of(run);
            for ps in &pipeline.slots {
                if let (Some(s), Some(cfg)) = (run.slots.get(&ps.slot), self.business.config.slots.get(&ps.slot)) {
                    slots.push((cfg.description.clone(), s.value.spoken()));
                }
            }
        }
        let intent = run.map(|r| r.intent.clone());
        let intent_description = intent.as_deref().and_then(|i| self.business.intent(i)).map(|i| i.description.clone());
        let customer_name = self.state.customer.as_ref().and_then(|c| c.name.clone());
        let mut text = format!("שיחה מועברת מ{}.", self.business.config.name);
        if let Some(name) = &customer_name {
            text.push_str(&format!(" הלקוח: {name}."));
        }
        if let Some(d) = &intent_description {
            text.push_str(&format!(" בקשה: {d}."));
        }
        for (label, value) in &slots {
            text.push_str(&format!(" {label}: {value}."));
        }
        let recent_start = self.state.history.len().saturating_sub(6);
        HandoffSummary {
            reason: reason.to_string(),
            business_id: self.business.config.id.clone(),
            intent,
            pipeline: run.map(|r| r.pipeline.clone()),
            slots,
            customer_name,
            recent: self.state.history[recent_start..].to_vec(),
            text,
        }
    }

    fn pipeline_of(&self, run: &PipelineRun) -> &PipelineConfig {
        // Runs are only ever created from validated pipeline ids.
        self.business.pipeline(&run.pipeline).expect("run refers to a validated pipeline")
    }

    fn render_plan(&mut self, response: &str, ctx: &RenderContext<'_>) -> Option<SpeechPlan> {
        let renderer = Renderer {
            business: &self.business,
            delivery_override: self.state.delivery.as_deref(),
            gain_db: self.state.gain_db,
        };
        renderer.render(response, ctx, &mut self.chooser, &mut self.state.last_variant)
    }

    fn render_ctx(&self, _result: Option<&serde_json::Value>) -> RenderContext<'static> {
        let mut ctx = RenderContext::default();
        if let Some(run) = &self.state.run {
            ctx.slots = run.slots.iter().map(|(k, v)| (k.clone(), v.value.clone())).collect();
        }
        if let Some(name) = self.state.customer.as_ref().and_then(|c| c.name.clone()) {
            ctx.extra.insert("customer_name".into(), name);
        }
        ctx
    }

    fn say(&mut self, out: &mut Out, response: &str, ctx: RenderContext<'_>, record: bool) {
        match self.render_plan(response, &ctx) {
            Some(plan) => out.speak(plan, record),
            None => tracing::error!(response, "response could not be rendered"),
        }
    }

    fn finish(&mut self, mut out: Out) -> Vec<Directive> {
        out.flush(false);
        if let Some(recorded) = out.recorded.take() {
            self.state.remember(Speaker::Agent, &recorded.text());
            self.state.last_plan = Some(recorded);
        }
        out.directives
    }
}

impl RenderContext<'_> {
    /// Detach from borrowed data (the customer is copied into `extra`).
    fn into_owned(self) -> RenderContext<'static> {
        let mut extra = self.extra;
        if let Some(c) = self.customer {
            if let Some(name) = c.name.clone() {
                extra.insert("customer_name".into(), name);
            }
            // The record's own values too ("last_from_city" for a returning caller's greeting).
            for (k, v) in &c.data {
                let value = v.as_str().map(str::to_string).or_else(|| v.as_i64().map(|n| n.to_string()));
                if let Some(value) = value {
                    extra.entry(k.clone()).or_insert(value);
                }
            }
        }
        RenderContext { slots: self.slots, result: None, customer: None, extra }
    }
}

/// The street and house number in a place whose city was not made out: "רמבם 12" of
/// "רמבם 12 בטרדיט" (up to the number), or the part before the comma of "רמבם 12, בטרדיט".
/// `None` when there is no street to tell from the rest ("אפרק").
fn street_before_city(raw: &str) -> Option<String> {
    let has_digit = |s: &str| s.chars().any(|c| c.is_ascii_digit());
    if let Some((street, rest)) = raw.split_once(',') {
        let (street, rest) = (street.trim(), rest.trim());
        return (has_digit(street) && !rest.is_empty()).then(|| street.to_string());
    }
    let words: Vec<&str> = raw.split_whitespace().collect();
    let at = words.iter().position(|w| w.chars().all(|c| c.is_ascii_digit()))?;
    (at > 0 && at + 1 < words.len()).then(|| words[..=at].join(" "))
}

/// An action and what it was asked, without its run id: the same question twice.
fn failed_key(action: &str, input: &serde_json::Value) -> String {
    format!("{action} {}", input.get("slots").unwrap_or(input))
}

/// How a number of passengers is said.
const PASSENGER_WORDS: [&str; 16] = [
    "אחד",
    "אחת",
    "שניים",
    "שתיים",
    "שלושה",
    "שלוש",
    "ארבעה",
    "ארבע",
    "חמישה",
    "חמש",
    "שישה",
    "שש",
    "שבעה",
    "שבע",
    "שמונה",
    "רק אני",
];

/// A digit or a number word in the words heard.
pub fn has_a_number(text: &str) -> bool {
    text.chars().any(|c| c.is_ascii_digit())
        || crate::text::normalize(text).split_whitespace().any(|w| {
            PASSENGER_WORDS.iter().any(|n| w == *n || w.strip_prefix('ו') == Some(n))
                || matches!(
                    w,
                    "תשעה"
                        | "תשע"
                        | "עשרה"
                        | "עשר"
                        | "לבד"
                        | "שנינו"
                        | "שלושתנו"
                        | "שתי"
                        | "שני"
                        | "שלושת"
                )
        })
}

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
use crate::state::{CallState, CompletedRun, Phase, PipelineRun, SlotState, Speaker, Step, Turn};
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
}

impl Engine {
    pub fn new(business: Arc<Business>, seed: u64) -> Self {
        let state = CallState::new(&business.config.id);
        Self { business, state, chooser: SeededChooser(seed | 1), offered_more: false, gazetteer: None }
    }

    /// The city whose street the question just asked for ("מאיזו עיר?" "בני ברק" ... "איזה
    /// רחוב?"): the runtime biases recognition with its streets. Only while the street is
    /// asked: kept on, a live call heard the caller's name as a street of that city.
    pub fn street_focus(&self) -> Option<String> {
        let run = self.state.run.as_ref()?;
        self.pipeline_of(run)
            .slots
            .iter()
            .filter(|ps| self.state.last_asks.contains(&ps.slot))
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
    /// street its city does not have): the runtime then holds the agent's words back, since
    /// they move on without it. The same checks as the turn itself, on a copy of the call: a
    /// live call's "מהשערה 18, אפרת" was refused only after "לאיזה רחוב נוסעים?" had played.
    pub fn rejects_any(&self, transcript: &str, fields: &[(String, String)]) -> bool {
        let mut probe = Engine {
            business: self.business.clone(),
            state: self.state.clone(),
            chooser: self.chooser.clone(),
            offered_more: self.offered_more,
            gazetteer: self.gazetteer.clone(),
        };
        probe.state.remember(Speaker::Caller, transcript);
        let fields = probe.answer_in_place(transcript, fields);
        !probe.apply_agent_fields(&fields).1.is_empty()
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
        let known = self.state.customer.as_ref().is_some_and(|c| c.name.is_some());
        let greeting = match (&self.business.config.customer_lookup, known) {
            (Some(cl), true) => cl.known_greeting.clone().unwrap_or_else(|| self.business.config.greeting.clone()),
            _ => self.business.config.greeting.clone(),
        };
        self.say(
            &mut out,
            &greeting,
            RenderContext { customer: self.state.customer.as_ref(), ..Default::default() }.into_owned(),
            true,
        );
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
        self.offered_more = false;
        self.state.remember(Speaker::Caller, transcript);
        // Notes were for the decision just made; new ones are for the next.
        self.state.agent_notes.clear();
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
                }
            }
        }
        let was_confirming = self.state.run.as_ref().is_some_and(|r| r.step == Step::AwaitingConfirmation);
        let fields = self.answer_in_place(transcript, &turn.fields);
        let (changed, rejected) = self.apply_agent_fields(&fields);
        // A detail changed after the read-back ("לא 40, 45"): read it back again, so the next
        // "יאללה" sends the corrected task instead of meeting another read-back.
        let action = if turn.action == AgentAction::None && changed && was_confirming {
            AgentAction::ReadBack
        } else {
            turn.action
        };
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
                if !spoken.trim_end().ends_with('?') {
                    // "רק כדי שלא תהיה טעות, כמה נוסעים? במונית עד עשרים."
                    let again = format!("ask_again_{slot}");
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
        let answered = spoken.is_empty() && action == AgentAction::None && {
            let asked = turn.phrase.as_deref().and_then(|p| self.slot_asked_by(p));
            asked.is_some_and(|slot| {
                turn.fields.iter().any(|(s, _)| *s == slot)
                    && self.state.run.as_ref().is_some_and(|r| r.slots.contains_key(&slot))
            })
        };
        if answered {
            tracing::info!(transcript, phrase = ?turn.phrase, "the agent asked again for what the caller just gave; asking the next question");
            self.next_question(&mut out);
            return self.finish(out);
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
            AgentAction::None => self.agent_say(&mut out, turn.phrase.as_deref(), &turn.say, spoken),
            AgentAction::Transfer => {
                self.agent_say(&mut out, turn.phrase.as_deref(), &turn.say, spoken);
                self.handoff(&mut out, "caller_requested");
            }
            AgentAction::EndCall => {
                if self.caller_said_goodbye(transcript) {
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
                let said_yes = !self.business.affirm.is_empty()
                    && self.business.affirm.find(&crate::text::normalize(transcript)).is_some();
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
            // The model said nothing: never leave the caller in silence.
            let r = self.business.config.fallback.ladder[0].clone();
            let ctx = self.render_ctx(None);
            self.say(&mut out, &r, ctx, true);
        }
        self.finish(out)
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
        if let Some(plan) = phrase.and_then(|p| self.render_response(p)) {
            out.speak(plan, true);
        }
        if phrase.is_some_and(|p| !crate::agent::say_after_phrase(&self.business, p, say)) {
            return;
        }
        if !say.trim().is_empty() {
            out.speak(SpeechPlan::free(say, &delivery, self.state.gain_db), true);
        }
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
        if crate::agent::phrase_ids(&self.business).contains(&response_id) {
            return format!(" (phrase {response_id})");
        }
        self.business
            .response(response_id)
            .and_then(|r| r.variants.first())
            .map(|v| format!(" (\"{v}\")"))
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
                ps.ask
                    .as_deref()
                    .is_some_and(|a| phrase == a || phrase == format!("{a}_street") || phrase == format!("{a}_city"))
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

    fn apply_agent_fields(&mut self, fields: &[(String, String)]) -> (bool, Vec<String>) {
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
        // What the second hearing heard counts as said too.
        let heard = match &self.state.second_hearing {
            Some(second) => format!("{heard}. {second}"),
            None => heard,
        };
        let fills: Vec<SlotFill> = fields
            .iter()
            .filter_map(|(slot, raw)| {
                let cfg = self.business.config.slots.get(slot)?;
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
    fn fill_defaults(&mut self, pipeline: &PipelineConfig) {
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
        let lookup = explicit.or(in_city_before).unwrap_or_else(|| g.resolve(spoken));
        Some(match lookup {
            // Asked once already, and the caller has no street: the locality is enough.
            Lookup::Found(a)
                if street_once
                    && !precise
                    && a.street.is_none()
                    && self.state.place_cities.get(slot) != Some(&a.city_said) =>
            {
                let ask = self.said_as(&self.street_question(slot));
                notes.push(format!(
                    "{slot} city {} is noted; now ask for the street there, once{ask}; if the caller does not know, \
                     pass the city again",
                    a.city_said
                ));
                self.state.place_cities.insert(slot.to_string(), a.city_said);
                return None;
            }
            Lookup::Found(a) if precise && a.street.is_none() => {
                notes.push(format!(
                    "{slot} city {} is noted; now ask for the street and house number (or a landmark) there",
                    a.city_said
                ));
                self.state.place_cities.insert(slot.to_string(), a.city_said);
                return None;
            }
            Lookup::Found(a) => {
                self.state.place_cities.remove(slot);
                SlotValue::Place { spoken: a.spoken(), address: Some(a.official()), customer_place: None }
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
            Lookup::NoStreet { city, heard, closest }
                if spoken.chars().any(|c| c.is_ascii_digit())
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
                let again = self.said_as("ask_again_street");
                notes.push(format!(
                    "{slot}: {city} has no street \"{heard}\"; it was not accepted, probably misheard. Ask for the \
                     street again{again}{hint}"
                ));
                self.state.place_cities.insert(slot.to_string(), city);
                rejected.push(slot.to_string());
                return None;
            }
            // "קניון הזהב, אלעד": neither a street nor a place on the list. Ask for its
            // address once; if the caller has none, it is taken as said, marked for the driver.
            Lookup::NoStreet { city, heard, closest } if self.state.doubted_streets.insert(slot.to_string()) => {
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
                self.say(out, &confirm.response, ctx, true);
            }
            // Nothing to confirm: go straight to the action.
            None => self.advance(out, false),
        }
    }

    /// The caller's own words close the call: a goodbye, "that's all", or a plain "no,
    /// thanks" after "anything else?". An LLM's reading of garbled speech is not enough.
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

        let applied = self.apply_fills(&u.slots);
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

    /// Business rules. Returns true when a rule took over the turn.
    fn apply_rules(&mut self, out: &mut Out) -> bool {
        let rules = self.business.config.rules.clone();
        for rule in rules {
            let Some(run) = &self.state.run else { return false };
            if rule.pipeline.as_ref().is_some_and(|p| *p != run.pipeline) || run.fired_rules.contains(&rule.id) {
                continue;
            }
            let value = run.slots.get(&rule.when.slot).map(|s| &s.value);
            let hit = match &rule.when.test {
                crate::config::ConditionTest::Gt(x) => value.and_then(SlotValue::as_f64).is_some_and(|v| v > *x),
                crate::config::ConditionTest::Lt(x) => value.and_then(SlotValue::as_f64).is_some_and(|v| v < *x),
                crate::config::ConditionTest::Eq(x) => value.is_some_and(|v| v.matches_json(x)),
                crate::config::ConditionTest::Present(p) => value.is_some() == *p,
            };
            if !hit {
                continue;
            }
            tracing::debug!(rule = %rule.id, "business rule fired");
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
            if let Some(run) = &mut self.state.run {
                run.step = Step::Executing { action_run: run_id };
                run.attempts = 1;
            }
            if let Some(filler) = &pipeline.filler {
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
                if let Some(r) = &pipeline.on_success {
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
                    if let Some(r) = &pipeline.on_unknown {
                        let ctx = self.render_ctx(None);
                        self.say(&mut out, r, ctx, true);
                    }
                    let result = json!({ "error": failure.error, "outcome_unknown": true, "run_id": run_id });
                    if self.business.handoff_number.is_some() {
                        self.handoff(&mut out, "action_outcome_unknown");
                        self.close_run("unknown", Some(result));
                    } else {
                        self.finish_run(&mut out, "unknown", Some(result));
                    }
                    return self.finish(out);
                }
                let error = failure.error;
                self.state.action_failures += 1;
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

    /// The caller said nothing for the configured silence.
    /// The caller said something that could not be made out (it was taken for line noise)
    /// and then nothing more: the question again ("כמה נוסעים?"), as a person would ask it,
    /// instead of the silence the caller would otherwise wait out. The owner wanted neither
    /// "I didn't hear" nor "say it again?". With no question pending, the business's first
    /// fallback.
    pub fn on_unheard(&mut self) -> Vec<Directive> {
        let mut out = Out::default();
        if self.state.phase != Phase::Active
            || matches!(self.state.run.as_ref().map(|r| &r.step), Some(Step::Executing { .. }))
        {
            return Vec::new();
        }
        // The question alone: "לאן נוסעים?", not the "סגור." said before it.
        let question = self.state.last_plan.as_ref().and_then(|p| {
            let last = p.segments.last().filter(|s| s.text.trim_end().ends_with('?'))?;
            let text = last.text.trim_end();
            let start = text[..text.len() - 1].rfind(['.', '!', '?']).map_or(0, |i| i + 1);
            Some(match text[start..].trim() {
                tail if start > 0 => SpeechPlan::free(tail, &last.delivery, p.gain_db),
                _ => SpeechPlan { segments: vec![last.clone()], gain_db: p.gain_db },
            })
        });
        match question {
            Some(question) => out.speak(question, false),
            None => {
                if let Some(r) = self.business.config.fallback.ladder.first().cloned() {
                    let ctx = self.render_ctx(None);
                    self.say(&mut out, &r, ctx, false);
                }
            }
        }
        self.finish(out)
    }

    pub fn on_silence(&mut self) -> Vec<Directive> {
        let mut out = Out::default();
        if self.state.phase != Phase::Active
            || matches!(self.state.run.as_ref().map(|r| &r.step), Some(Step::Executing { .. }))
        {
            return Vec::new();
        }
        self.state.silence_reprompts += 1;
        let silence = self.business.config.silence.clone();
        if self.state.silence_reprompts > silence.max_reprompts {
            self.goodbye(&mut out);
            return self.finish(out);
        }
        match &silence.response {
            Some(r) => {
                let ctx = self.render_ctx(None);
                self.say(&mut out, r, ctx, false);
                if let Some(last) = self.state.last_plan.clone() {
                    out.speak(last, false);
                }
            }
            None => {
                if let Some(last) = self.state.last_plan.clone() {
                    out.speak(last, false);
                }
            }
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
            MetaIntent::Wait => {}
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
        if self.business.handoff_number.is_some() {
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
        match after {
            AfterPipeline::Continue => self.offer_more(out),
            AfterPipeline::End => self.goodbye(out),
        }
    }

    fn ask(&mut self, out: &mut Out, slot: &str, acknowledge: bool) {
        let Some(run) = &self.state.run else { return };
        let pipeline = self.pipeline_of(run);
        let Some(mut ask) = pipeline.slots.iter().find(|p| p.slot == slot).and_then(|p| p.ask.clone()) else { return };
        // The city is known: only the street is missing ("לאיזה רחוב?", not "לאן?").
        let street = format!("{ask}_street");
        if self.state.place_cities.contains_key(slot) && self.business.response(&street).is_some() {
            ask = street;
        }
        let has_prefix = self.business.response(&ask).is_some_and(|r| r.prefix.is_some());
        if acknowledge && !has_prefix {
            if let Some(ack) = self.business.config.acknowledgement.clone() {
                let ctx = self.render_ctx(None);
                self.say(out, &ack, ctx, true);
            }
        }
        let ctx = self.render_ctx(None);
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
        if let Some(name) = self.customer.and_then(|c| c.name.clone()) {
            extra.insert("customer_name".into(), name);
        }
        RenderContext { slots: self.slots, result: None, customer: None, extra }
    }
}

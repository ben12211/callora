//! Explicit call state. This, not an LLM transcript, is the source of truth for what the
//! caller wants and what has been collected.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::address_form::AddressForm;
use crate::customer::Customer;
use crate::render::SpeechPlan;
use crate::values::{Provenance, SlotValue};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlotState {
    pub value: SlotValue,
    pub confidence: f32,
    pub provenance: Provenance,
    /// The caller explicitly agreed to it (single-slot or full read-back).
    pub confirmed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum Step {
    /// Collecting slots; `awaiting` is the slot just asked for, if any.
    Collecting { awaiting: Option<String> },
    /// Reading one uncertain value back ("ז'בוטינסקי ברמת גן, נכון?").
    ConfirmingSlot { slot: String },
    /// Read everything back; waiting for yes/no.
    AwaitingConfirmation,
    /// The business action is running.
    Executing { action_run: u64 },
}

/// One undoable change, for "go back".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub slot: String,
    pub previous: Option<SlotState>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineRun {
    pub pipeline: String,
    pub intent: String,
    pub slots: BTreeMap<String, SlotState>,
    pub step: Step,
    pub journal: Vec<JournalEntry>,
    /// The full read-back was accepted.
    pub confirmed: bool,
    pub attempts: u32,
    /// Rules that already fired in this run (each fires once per value change).
    #[serde(default)]
    pub fired_rules: Vec<String>,
}

impl PipelineRun {
    pub fn new(pipeline: &str, intent: &str) -> Self {
        Self {
            pipeline: pipeline.to_string(),
            intent: intent.to_string(),
            slots: BTreeMap::new(),
            step: Step::Collecting { awaiting: None },
            journal: Vec::new(),
            confirmed: false,
            attempts: 0,
            fired_rules: Vec::new(),
        }
    }

    pub fn awaiting_slot(&self) -> Option<&str> {
        match &self.step {
            Step::Collecting { awaiting } => awaiting.as_deref(),
            Step::ConfirmingSlot { slot } => Some(slot.as_str()),
            _ => None,
        }
    }

    /// Set a slot, journaling the previous value. A changed value invalidates an earlier
    /// read-back confirmation.
    pub fn set(&mut self, slot: &str, state: SlotState) {
        let previous = self.slots.insert(slot.to_string(), state.clone());
        if previous.as_ref().map(|p| &p.value) != Some(&state.value) {
            self.confirmed = false;
            self.fired_rules.clear();
        }
        self.journal.push(JournalEntry { slot: slot.to_string(), previous });
    }

    pub fn clear(&mut self, slot: &str) {
        if let Some(previous) = self.slots.remove(slot) {
            self.journal.push(JournalEntry { slot: slot.to_string(), previous: Some(previous) });
            self.confirmed = false;
        }
    }

    /// Undo the last change. Returns the slot that changed.
    pub fn undo(&mut self) -> Option<String> {
        let entry = self.journal.pop()?;
        match entry.previous {
            Some(prev) => {
                self.slots.insert(entry.slot.clone(), prev);
            }
            None => {
                self.slots.remove(&entry.slot);
            }
        }
        self.confirmed = false;
        Some(entry.slot)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Active,
    HandingOff,
    Ending,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    pub speaker: Speaker,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Speaker {
    Caller,
    Agent,
}

/// A finished pipeline run, kept for handoff context and for the call record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletedRun {
    pub pipeline: String,
    pub outcome: String,
    pub slots: BTreeMap<String, SlotValue>,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
}

/// One of the business's responses, with the values it fills in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Prompt {
    pub response: String,
    #[serde(default)]
    pub values: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallState {
    pub business_id: String,
    pub phase: Phase,
    pub run: Option<PipelineRun>,
    /// Runs paused by an intent switch, resumed afterwards (most recent last).
    pub suspended: Vec<PipelineRun>,
    pub completed: Vec<CompletedRun>,
    pub last_plan: Option<SpeechPlan>,
    pub fallback_level: u32,
    /// Times the fallback ladder was started over instead of ending the call.
    #[serde(default)]
    pub fallback_restarts: u32,
    pub action_failures: u32,
    pub silence_reprompts: u32,
    /// Sticky delivery override ("slow" after "speak slower", "calm" when frustrated).
    pub delivery: Option<String>,
    pub gain_db: f32,
    pub customer: Option<Customer>,
    pub last_variant: BTreeMap<String, usize>,
    pub turns: u32,
    /// Bounded transcript, for handoff context and LLM hints. Never drives state.
    pub history: Vec<Turn>,
    /// What the system could not accept from the agent's last decision, told to it on the
    /// next turn ("passengers \"42\": out of range").
    #[serde(default)]
    pub agent_notes: Vec<String>,
    /// The locality given for a place slot that still needs its street ("pickup" → "אלעד").
    #[serde(default)]
    pub place_cities: BTreeMap<String, String>,
    /// Place slots whose street was not found once already: the second time it is kept.
    #[serde(default)]
    pub doubted_streets: BTreeSet<String>,
    /// How many times each slot's place was rejected as never said: the third time the agent
    /// insists, it is taken (a check that loops is worse than a doubtful place).
    #[serde(default)]
    pub unheard_rejections: BTreeMap<String, u8>,
    /// Optional slots already asked before a read-back.
    #[serde(default)]
    pub asked_before_confirm: BTreeSet<String>,
    /// Required details asked for and not given yet, in the order asked: the call does not
    /// move on to other questions until each is given (or the task changes).
    #[serde(default)]
    pub open_questions: Vec<String>,
    /// The details the last question asked for.
    #[serde(default)]
    pub last_asks: Vec<String>,
    /// A street and number given without its city ("בן זכאי 45"), kept until the city
    /// comes: slot → the words.
    #[serde(default)]
    pub place_streets: BTreeMap<String, String>,
    /// What to ask about a place that was not taken ("לא מצאתי את ארנוביץ בירושלים. התכוונת
    /// ל…?"): slot → the business's responses to say, with their values.
    #[serde(default)]
    pub doubt_confirm: BTreeMap<String, Vec<Prompt>>,
    /// A street given without its house number, asked for on its own: slot → (street, city).
    #[serde(default)]
    pub place_numbers: BTreeMap<String, (String, String)>,
    /// Places whose house number was asked once: the second time the street is taken as is.
    #[serde(default)]
    pub number_asked: BTreeSet<String>,
    /// How many times each place was not taken: at the third, a person takes the call.
    #[serde(default)]
    pub place_rejections: BTreeMap<String, u8>,
    /// This turn's second transcript of the caller's words (see the runtime's second hearing).
    #[serde(default)]
    pub second_hearing: Option<String>,
    /// This turn's words began before the agent's last reply started to play: they finish
    /// the caller's answer to the question before it ("דוד" ... "אביטבול").
    #[serde(default)]
    pub continues_answer: bool,
    /// Masculine or feminine once the caller's words show it ("אני צריכה"); neutral until then.
    #[serde(default)]
    pub address_form: AddressForm,
    /// The number the caller is calling from, when the network gives it.
    #[serde(default)]
    pub caller_phone: Option<String>,
    pub next_action_run: u64,
    /// The caller asked to wait ("רגע"): the silence reprompt waits longer.
    #[serde(default)]
    pub waiting: bool,
    /// The house number said with a street that was not found in its city: kept for the same
    /// street in the city the caller names next.
    #[serde(default)]
    pub doubted_numbers: BTreeMap<String, String>,
    /// Streets the system offered ("התכוונת לרבן יוחנן בן זכאי?"): a "yes" takes one without
    /// the caller having said its words.
    #[serde(default)]
    pub offered_streets: Vec<String>,
    /// Every detail a question has asked for in this call.
    #[serde(default)]
    pub asked_slots: BTreeSet<String>,
    /// The caller turn at which the agent last said the line is noisy: said once per turn,
    /// not every time noise cuts in.
    #[serde(default)]
    pub noise_apology_turn: Option<u32>,
}

pub const HISTORY_LIMIT: usize = 24;

impl CallState {
    pub fn new(business_id: &str) -> Self {
        Self {
            business_id: business_id.to_string(),
            phase: Phase::Active,
            run: None,
            suspended: Vec::new(),
            completed: Vec::new(),
            last_plan: None,
            fallback_level: 0,
            fallback_restarts: 0,
            action_failures: 0,
            silence_reprompts: 0,
            delivery: None,
            gain_db: 0.0,
            customer: None,
            last_variant: BTreeMap::new(),
            turns: 0,
            history: Vec::new(),
            agent_notes: Vec::new(),
            place_cities: BTreeMap::new(),
            doubted_streets: BTreeSet::new(),
            unheard_rejections: BTreeMap::new(),
            asked_before_confirm: BTreeSet::new(),
            open_questions: Vec::new(),
            last_asks: Vec::new(),
            place_streets: BTreeMap::new(),
            doubt_confirm: BTreeMap::new(),
            place_numbers: BTreeMap::new(),
            number_asked: BTreeSet::new(),
            place_rejections: BTreeMap::new(),
            second_hearing: None,
            continues_answer: false,
            address_form: AddressForm::Unknown,
            caller_phone: None,
            next_action_run: 1,
            waiting: false,
            doubted_numbers: BTreeMap::new(),
            offered_streets: Vec::new(),
            asked_slots: BTreeSet::new(),
            noise_apology_turn: None,
        }
    }

    /// How many times in a row, up to now, the agent asked the very same question
    /// ("כמה נוסעים?" eleven times in a live call while the caller answered other things).
    pub fn same_question_streak(&self) -> usize {
        let asked: Vec<String> = self
            .history
            .iter()
            .filter(|t| t.speaker == Speaker::Agent)
            .map(|t| crate::text::normalize(t.text.rsplit(['.', '!']).next().unwrap_or(&t.text)))
            .collect();
        let Some(last) = asked.last().filter(|q| !q.is_empty()) else { return 0 };
        if !self
            .history
            .iter()
            .rev()
            .find(|t| t.speaker == Speaker::Agent)
            .is_some_and(|t| t.text.trim_end().ends_with('?'))
        {
            return 0;
        }
        asked.iter().rev().take_while(|q| *q == last).count()
    }

    pub fn remember(&mut self, speaker: Speaker, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        if speaker == Speaker::Caller {
            // A later clear cue wins: the caller may correct it ("אני צריכה", "בלשון נקבה").
            if let Some(form) = crate::address_form::detect(text).filter(|f| *f != self.address_form) {
                tracing::info!(?form, "address form");
                self.address_form = form;
            }
        }
        self.history.push(Turn { speaker, text: text.to_string() });
        if self.history.len() > HISTORY_LIMIT {
            let excess = self.history.len() - HISTORY_LIMIT;
            self.history.drain(..excess);
        }
    }
}

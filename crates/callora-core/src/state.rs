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
    /// Masculine or feminine once the caller's words show it ("אני צריכה"); neutral until then.
    #[serde(default)]
    pub address_form: AddressForm,
    /// The number the caller is calling from, when the network gives it.
    #[serde(default)]
    pub caller_phone: Option<String>,
    pub next_action_run: u64,
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
            address_form: AddressForm::Unknown,
            caller_phone: None,
            next_action_run: 1,
        }
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

//! The Business JSON schema.
//!
//! Everything a business *means* lives here: its intents, pipelines, slots, rules, actions,
//! responses and voice. Nothing here says *how* the runtime executes it. Unknown fields are
//! rejected everywhere, so a typo in a business file fails loading instead of silently
//! turning a behaviour off.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;

pub type IntentId = String;
pub type PipelineId = String;
pub type SlotId = String;
pub type ActionId = String;
pub type ResponseId = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BusinessConfig {
    pub schema_version: u32,
    /// Stable identifier, `[a-z0-9_-]+`. Used in storage paths, metrics and logs.
    pub id: String,
    pub name: String,
    /// BCP-47 locale, e.g. `he-IL`.
    pub language: String,
    /// IANA time zone, e.g. `Asia/Jerusalem`.
    pub timezone: String,
    /// E.164 numbers this business answers on.
    #[serde(default)]
    pub phone_numbers: Vec<String>,
    /// Name of an environment variable holding more E.164 numbers, comma separated. Keeps
    /// real numbers out of source control.
    #[serde(default)]
    pub phone_numbers_env: Option<String>,
    pub voice: VoiceConfig,
    /// Response played the moment the call connects.
    pub greeting: ResponseId,
    pub lexicon: Lexicon,
    #[serde(default)]
    pub meta_intents: BTreeMap<String, MetaIntentConfig>,
    pub intents: Vec<IntentConfig>,
    pub slots: BTreeMap<SlotId, SlotConfig>,
    #[serde(default)]
    pub places: Vec<PlaceConfig>,
    /// Words the speech recognizer should expect, such as the cities and streets of the
    /// service area. Known place names and aliases are added automatically.
    #[serde(default)]
    pub stt_keyterms: Vec<String>,
    pub pipelines: BTreeMap<PipelineId, PipelineConfig>,
    #[serde(default)]
    pub actions: BTreeMap<ActionId, ActionConfig>,
    pub responses: BTreeMap<ResponseId, ResponseConfig>,
    #[serde(default)]
    pub rules: Vec<RuleConfig>,
    #[serde(default)]
    pub pronunciations: BTreeMap<String, String>,
    pub fallback: FallbackConfig,
    pub handoff: HandoffConfig,
    #[serde(default)]
    pub silence: SilenceConfig,
    #[serde(default)]
    pub customer_lookup: Option<CustomerLookupConfig>,
    #[serde(default)]
    pub understanding: UnderstandingConfig,
    /// Response used when the business intent has nothing else to do ("anything else?").
    pub anything_else: ResponseId,
    /// Reads a single uncertain value back; must use `{value}` ("{value}, נכון?").
    pub confirm_slot: ResponseId,
    /// Short acknowledgements played before dynamic content ("סגור.", "אוקיי.").
    #[serde(default)]
    pub acknowledgement: Option<ResponseId>,
    /// Played if the call ends by the business (after a completed flow, or on goodbye).
    pub goodbye: ResponseId,
}

// ---------------------------------------------------------------------------------------
// Voice

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceConfig {
    /// TTS provider used for the voice library and for dynamic fallback.
    pub provider: TtsProviderKind,
    /// Voice id, directly. Prefer `voice_id_env` for ids you do not want in the repo.
    #[serde(default)]
    pub voice_id: Option<String>,
    #[serde(default)]
    pub voice_id_env: Option<String>,
    /// Model used to generate the pre-recorded library (quality first).
    pub library_model: String,
    /// Model used for dynamic TTS during a call (latency first).
    pub dynamic_model: String,
    /// Free-form personality descriptors (young, Israeli, friendly...). Documentation for
    /// voice casting and for the LLM understanding prompt; not interpreted by the engine.
    #[serde(default)]
    pub personality: Vec<String>,
    pub settings: VoiceSettings,
    /// Named delivery styles (normal, calm, important, quick, slow). `normal` is required.
    pub deliveries: BTreeMap<String, DeliveryConfig>,
    /// Pre-generated opener ("אוקיי.") played the moment a reply must start with live TTS,
    /// so the caller hears something during the second the synthesis takes.
    #[serde(default)]
    pub dynamic_cover: Option<ResponseId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TtsProviderKind {
    Elevenlabs,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceSettings {
    pub stability: f32,
    pub similarity_boost: f32,
    #[serde(default)]
    pub style: f32,
    pub speed: f32,
}

/// Overrides applied on top of [`VoiceSettings`] for one delivery style.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryConfig {
    #[serde(default)]
    pub stability: Option<f32>,
    #[serde(default)]
    pub similarity_boost: Option<f32>,
    #[serde(default)]
    pub style: Option<f32>,
    #[serde(default)]
    pub speed: Option<f32>,
    /// Pre-generate every static response in this delivery too (e.g. `slow` for repeats).
    #[serde(default)]
    pub pregenerate: bool,
}

impl VoiceConfig {
    pub fn settings_for(&self, delivery: &str) -> VoiceSettings {
        let mut s = self.settings;
        if let Some(d) = self.deliveries.get(delivery) {
            if let Some(v) = d.stability {
                s.stability = v;
            }
            if let Some(v) = d.similarity_boost {
                s.similarity_boost = v;
            }
            if let Some(v) = d.style {
                s.style = v;
            }
            if let Some(v) = d.speed {
                s.speed = v;
            }
        }
        s
    }
}

// ---------------------------------------------------------------------------------------
// Understanding vocabulary

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lexicon {
    /// Phrases that mean "yes" when a confirmation is pending.
    pub affirm: Vec<String>,
    /// Phrases that mean "no".
    pub deny: Vec<String>,
    /// Words carrying no content ("אה", "אממ", "רגע"); ignored for coverage.
    #[serde(default)]
    pub fillers: Vec<String>,
    /// Phrases that mean "now" for time slots.
    #[serde(default)]
    pub now: Vec<String>,
}

/// The meta intents the runtime understands. Their *behaviour* is built in; their
/// *vocabulary* and *responses* come from the business.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetaIntent {
    RepeatLast,
    DidNotUnderstand,
    SpeakSlower,
    SpeakLouder,
    CancelCurrentFlow,
    GoBack,
    TransferHuman,
    Goodbye,
    /// "רגע", "שנייה": the caller wants the agent to stop and wait. Nothing is said.
    Wait,
}

impl MetaIntent {
    pub const ALL: [MetaIntent; 9] = [
        MetaIntent::RepeatLast,
        MetaIntent::DidNotUnderstand,
        MetaIntent::SpeakSlower,
        MetaIntent::SpeakLouder,
        MetaIntent::CancelCurrentFlow,
        MetaIntent::GoBack,
        MetaIntent::TransferHuman,
        MetaIntent::Goodbye,
        MetaIntent::Wait,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            MetaIntent::RepeatLast => "repeat_last",
            MetaIntent::DidNotUnderstand => "did_not_understand",
            MetaIntent::SpeakSlower => "speak_slower",
            MetaIntent::SpeakLouder => "speak_louder",
            MetaIntent::CancelCurrentFlow => "cancel_current_flow",
            MetaIntent::GoBack => "go_back",
            MetaIntent::TransferHuman => "transfer_human",
            MetaIntent::Goodbye => "goodbye",
            MetaIntent::Wait => "wait",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == value)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetaIntentConfig {
    /// Matches anywhere in the utterance.
    #[serde(default)]
    pub phrases: Vec<String>,
    /// Matches only when it is (almost) the whole utterance, e.g. "מה?".
    #[serde(default)]
    pub exact: Vec<String>,
    /// Response to speak, where the meta intent speaks one of its own.
    #[serde(default)]
    pub response: Option<ResponseId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntentConfig {
    pub id: IntentId,
    /// One line for the LLM understanding prompt and for operators.
    pub description: String,
    /// Words/phrases that indicate this intent on the fast path.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Example utterances; used in the LLM prompt and in config tests.
    #[serde(default)]
    pub examples: Vec<String>,
    /// What the intent does. Exactly one of these is set.
    #[serde(default)]
    pub pipeline: Option<PipelineId>,
    #[serde(default)]
    pub respond: Option<ResponseId>,
    #[serde(default)]
    pub handoff: bool,
    /// Minimum confidence to switch away from an active pipeline to this intent.
    #[serde(default = "default_switch_confidence")]
    pub switch_confidence: f32,
}

fn default_switch_confidence() -> f32 {
    0.75
}

// ---------------------------------------------------------------------------------------
// Slots

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlotConfig {
    #[serde(rename = "type")]
    pub kind: SlotKind,
    pub description: String,
    /// Regexes (Rust syntax) run against the match-normalized utterance. Each needs a
    /// named group `value`. Matched anywhere in the utterance, in any state.
    #[serde(default)]
    pub patterns: Vec<String>,
    /// Slots are extracted in ascending priority; each match hides its words from later
    /// slots, so specific patterns (a passenger count) run before greedy ones (a place).
    #[serde(default = "default_priority")]
    pub priority: u32,
    /// Hebrew prefix letters to strip from a bare answer ("מרבי עקיבא" → "רבי עקיבא") when
    /// the utterance answers the question for this slot.
    #[serde(default)]
    pub strip_prefixes: Vec<String>,
    /// Phrases that resolve against the customer record: alias → customer place key.
    /// e.g. `{"מהבית": "home"}`.
    #[serde(default)]
    pub context_aliases: BTreeMap<String, String>,
    /// Integer bounds.
    #[serde(default)]
    pub min: Option<i64>,
    #[serde(default)]
    pub max: Option<i64>,
    /// Allowed values for `enum`: canonical → phrases.
    #[serde(default)]
    pub values: BTreeMap<String, Vec<String>>,
    /// Phrases that stand for a value directly: `{"לבד": 1, "זוג": 2}`.
    #[serde(default)]
    pub synonyms: BTreeMap<String, serde_json::Value>,
    /// Below this confidence the engine reads the value back ("ז'בוטינסקי ברמת גן, נכון?").
    #[serde(default = "default_confirm_below")]
    pub confirm_below: f32,
    /// Below this confidence the value is not accepted at all; the engine asks again.
    #[serde(default = "default_reject_below")]
    pub reject_below: f32,
}

fn default_priority() -> u32 {
    50
}
fn default_confirm_below() -> f32 {
    0.6
}
fn default_reject_below() -> f32 {
    0.35
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotKind {
    Text,
    /// A location: free address, a gazetteer place, or a customer-context alias.
    Place,
    Integer,
    Boolean,
    /// "now", "in 10 minutes", "08:30", "tomorrow at 7".
    Time,
    Enum,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaceConfig {
    /// Canonical spoken form, e.g. "נתב״ג".
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    /// What gets sent to the business action (a full address), if different from `name`.
    #[serde(default)]
    pub address: Option<String>,
}

// ---------------------------------------------------------------------------------------
// Pipelines

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineConfig {
    pub description: String,
    pub slots: Vec<PipelineSlot>,
    /// Read everything back and wait for yes/no before running the action.
    #[serde(default)]
    pub confirm: Option<ConfirmConfig>,
    /// The business action this pipeline exists to run.
    #[serde(default)]
    pub action: Option<ActionId>,
    /// Played immediately when the action starts, to cover its latency.
    #[serde(default)]
    pub filler: Option<ResponseId>,
    /// Spoken after a successful action; may reference the action's result fields.
    #[serde(default)]
    pub on_success: Option<ResponseId>,
    /// Spoken when the action fails for good (after retries), before any handoff.
    #[serde(default)]
    pub on_failure: Option<ResponseId>,
    /// Spoken when the pipeline has no action and all slots are filled.
    #[serde(default)]
    pub on_complete: Option<ResponseId>,
    /// What to do once the pipeline finishes.
    #[serde(default)]
    pub after: AfterPipeline,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PipelineSlot {
    pub slot: SlotId,
    #[serde(default)]
    pub required: bool,
    /// Question asking for this slot. Required when `required` and there is no default.
    #[serde(default)]
    pub ask: Option<ResponseId>,
    /// Value assumed when the caller never says otherwise, as the caller would say it
    /// (e.g. "now", "1").
    #[serde(default)]
    pub default: Option<serde_json::Value>,
    /// Take the value from the customer record when known (e.g. `home` for pickup).
    #[serde(default)]
    pub from_customer: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmConfig {
    pub response: ResponseId,
    /// Asked after the caller says "no" to the read-back.
    pub ask_change: ResponseId,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AfterPipeline {
    /// Ask "anything else?" and keep listening.
    #[default]
    Continue,
    /// Say goodbye and hang up.
    End,
}

// ---------------------------------------------------------------------------------------
// Actions

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionConfig {
    pub description: String,
    /// Backends tried in order; the first one that is configured is used.
    pub backends: Vec<ActionBackend>,
    /// Refuse to run without an explicit "yes" to a read-back.
    #[serde(default)]
    pub requires_confirmation: bool,
    /// Lowest acceptable confidence of any slot the action consumes.
    #[serde(default)]
    pub min_confidence: f32,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Total attempts including the first. Only for idempotent or keyed actions.
    #[serde(default = "default_attempts")]
    pub max_attempts: u32,
}

fn default_timeout_ms() -> u64 {
    4000
}
fn default_attempts() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActionBackend {
    /// POST `{action, business, call, input}` as JSON; the JSON reply is the result.
    Http {
        /// Environment variable holding the endpoint URL. Unset → backend unavailable.
        url_env: String,
        /// Optional environment variable holding a bearer token.
        #[serde(default)]
        token_env: Option<String>,
    },
    /// A fixed result. For demos, tests, and businesses not yet integrated.
    Mock {
        result: serde_json::Value,
        #[serde(default)]
        latency_ms: u64,
        /// Return this error instead of a result.
        #[serde(default)]
        error: Option<String>,
    },
}

// ---------------------------------------------------------------------------------------
// Responses

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseConfig {
    /// Alternatives with the same meaning; one is chosen per use, never the same twice in
    /// a row. May contain `{placeholders}`.
    pub variants: Vec<String>,
    /// Typed placeholders. Undeclared placeholders are free text (dynamic TTS).
    #[serde(default)]
    pub params: BTreeMap<String, ParamConfig>,
    /// Another response spoken first as its own audio segment (usually an acknowledgement).
    #[serde(default)]
    pub prefix: Option<ResponseId>,
    /// Delivery style name (see `voice.deliveries`). Defaults to `normal`.
    #[serde(default)]
    pub delivery: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ParamConfig {
    /// A counted noun with Hebrew agreement: 1 → `singular`, n → "<number> <plural>".
    Count {
        gender: Gender,
        singular: String,
        plural: String,
        /// Inclusive range that the voice library pre-generates.
        range: [i64; 2],
    },
    /// A bare number in words.
    Number { gender: Gender, range: [i64; 2] },
    /// One of a fixed set of spoken values: canonical value → spoken text.
    Enum { values: BTreeMap<String, String> },
    /// A time slot value spoken naturally ("עכשיו", "בעוד עשר דקות", "בשמונה וחצי").
    Time,
    /// Free text. Never pre-generated.
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gender {
    Masculine,
    Feminine,
}

// ---------------------------------------------------------------------------------------
// Rules

/// A business rule, evaluated whenever slots change inside a pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleConfig {
    pub id: String,
    /// Restrict to one pipeline; `None` means every pipeline.
    #[serde(default)]
    pub pipeline: Option<PipelineId>,
    pub when: Condition,
    pub then: RuleEffect,
}

// No `deny_unknown_fields`: serde does not support it together with `flatten`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Condition {
    pub slot: SlotId,
    #[serde(flatten)]
    pub test: ConditionTest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConditionTest {
    Gt(f64),
    Lt(f64),
    Eq(serde_json::Value),
    Present(bool),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "do", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuleEffect {
    /// Say something, clear the slot, and ask for it again.
    Reject { response: ResponseId },
    /// Say something and hand the call to a human.
    Handoff { response: Option<ResponseId>, reason: String },
    /// Set another slot.
    Set { slot: SlotId, value: serde_json::Value },
}

// ---------------------------------------------------------------------------------------
// Fallback, handoff, silence, customer

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FallbackConfig {
    /// Escalating responses for consecutive misunderstandings. After the last one the call
    /// is handed off (if possible) or ended.
    pub ladder: Vec<ResponseId>,
    /// Said instead of ending the call when the ladder runs out and no human can take over;
    /// the ladder then starts again, once. A bad line should not cost the caller the call.
    #[serde(default)]
    pub restart: Option<ResponseId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoffConfig {
    /// Environment variable holding the E.164 number of the human desk.
    #[serde(default)]
    pub phone_number_env: Option<String>,
    /// Spoken before transferring.
    pub response: ResponseId,
    /// Spoken when a handoff is wanted but no human desk is configured.
    pub unavailable_response: ResponseId,
    /// Hand off after this many failed action runs within a call.
    #[serde(default = "default_action_failures")]
    pub after_action_failures: u32,
}

fn default_action_failures() -> u32 {
    2
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SilenceConfig {
    /// Silence after the agent finished speaking before a reprompt.
    pub reprompt_after_ms: u64,
    /// Reprompts before giving up and ending the call.
    pub max_reprompts: u32,
    /// Response for the reprompt. `None` repeats the last response.
    #[serde(default)]
    pub response: Option<ResponseId>,
}

impl Default for SilenceConfig {
    fn default() -> Self {
        Self { reprompt_after_ms: 7000, max_reprompts: 2, response: None }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomerLookupConfig {
    /// Action called with `{phone}` at call start; returns a customer record.
    pub action: ActionId,
    /// Greeting used instead of the default when the customer is known; may use `{name}`.
    #[serde(default)]
    pub known_greeting: Option<ResponseId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnderstandingConfig {
    /// Use the LLM when the fast path explains less than this fraction of the utterance.
    #[serde(default = "default_coverage")]
    pub llm_below_coverage: f32,
    /// Hard ceiling for the LLM call; past it the fast-path result is used.
    #[serde(default = "default_llm_timeout")]
    pub llm_timeout_ms: u64,
    /// Filler spoken when the LLM is consulted and is slower than `filler_after_ms`.
    #[serde(default)]
    pub thinking_filler: Option<ResponseId>,
    #[serde(default = "default_filler_after")]
    pub filler_after_ms: u64,
}

fn default_coverage() -> f32 {
    0.6
}
fn default_llm_timeout() -> u64 {
    1500
}
fn default_filler_after() -> u64 {
    450
}

impl Default for UnderstandingConfig {
    fn default() -> Self {
        Self {
            llm_below_coverage: default_coverage(),
            llm_timeout_ms: default_llm_timeout(),
            thinking_filler: None,
            filler_after_ms: default_filler_after(),
        }
    }
}

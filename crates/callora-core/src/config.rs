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
    /// Words that mark a spot named the way people talk, with no address behind it ("כניסה
    /// לעיר", "הצומת", "סינמה סיטי"): such a place is taken as the caller said it, with its
    /// city, and the caller is not asked for an address it does not have.
    #[serde(default)]
    pub informal_places: Vec<String>,
    /// Words of a place of the caller's own ("הבית", "העבודה", "אמא שלי", "הגן של הילד"): no
    /// address, and none the system knows unless the customer's record has it. The agent asks
    /// where it is in its own words; the system does not answer with "לא הכרתי את הבית".
    #[serde(default)]
    pub personal_places: Vec<String>,
    /// Words the speech recognizer should expect, such as the cities and streets of the
    /// service area. Known place names and aliases are added automatically.
    #[serde(default)]
    pub stt_keyterms: Vec<String>,
    /// The towns the business mostly serves, the most frequent first. Recognition is told of
    /// them from the first word of a call, and a word that sounds like one of them is taken
    /// for it more loosely than for the rest of the country ("מלאד", "בלד" for אלעד;
    /// "לבנברג" for בני ברק): one rule for every town of the area, not one per mishearing.
    #[serde(default)]
    pub service_area: Vec<String>,
    pub pipelines: BTreeMap<PipelineId, PipelineConfig>,
    #[serde(default)]
    pub actions: BTreeMap<ActionId, ActionConfig>,
    pub responses: BTreeMap<ResponseId, ResponseConfig>,
    #[serde(default)]
    pub rules: Vec<RuleConfig>,
    #[serde(default)]
    pub pronunciations: BTreeMap<String, String>,
    /// Overrides for a caller addressed in feminine ("לך" → "לָךְ" instead of "לְךָ").
    #[serde(default)]
    pub pronunciations_feminine: BTreeMap<String, String>,
    pub fallback: FallbackConfig,
    pub handoff: HandoffConfig,
    #[serde(default)]
    pub silence: SilenceConfig,
    #[serde(default)]
    pub customer_lookup: Option<CustomerLookupConfig>,
    #[serde(default)]
    pub understanding: UnderstandingConfig,
    /// When set (and an LLM is configured), an LLM agent runs the conversation: it decides
    /// every reply and next step, and the engine enforces the business's hard rules.
    #[serde(default)]
    pub agent: Option<AgentConfig>,
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
    /// How much faster than the model speaks the caller hears it, at the same pitch (1.25:
    /// a quarter faster). For models that ignore `speed`: eleven_v4_turbo speaks about 40%
    /// slower than eleven_v3 ("נורא איטי, נמרח"). The library and live speech alike.
    #[serde(default = "default_tempo")]
    pub tempo: f32,
    /// Voices the owner may switch to from the settings page. The voice in use by default
    /// is still `voice_id_env`'s; a switch builds the library in the chosen voice first.
    #[serde(default)]
    pub choices: Vec<VoiceChoice>,
}

/// A voice offered on the settings page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceChoice {
    /// ElevenLabs voice id.
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// A sample to listen to (https), when the voice has one.
    #[serde(default)]
    pub preview: Option<String>,
}

fn default_tempo() -> f32 {
    1.0
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
    /// ElevenLabs' speaker boost (clearer, a little more like the original voice, a little
    /// slower). Left out of the saved settings when true so existing libraries stay valid.
    #[serde(default = "default_speaker_boost", skip_serializing_if = "is_true")]
    pub speaker_boost: bool,
}

fn default_speaker_boost() -> bool {
    true
}

fn is_true(v: &bool) -> bool {
    *v
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
    /// Words of a caller correcting a detail already given ("לא", "טעיתי"): only then does a
    /// detail change while the question was about another.
    #[serde(default)]
    pub correct: Vec<String>,
    /// Complete phrases with a correction word that correct nothing ("סליחה", "לא צריך כלום"): a yes
    /// with one of them in it ("כן, לא צריך כלום") is still a yes.
    #[serde(default)]
    pub harmless: Vec<String>,
    /// A caller checking the line is still there ("הלו"): answered, never taken for noise.
    #[serde(default)]
    pub hello: Vec<String>,
    /// "Nothing" ("אין", "כלום"): with a "no", the answer to an optional question, not its value.
    #[serde(default)]
    pub nothing: Vec<String>,
    /// Words that announce a transfer to a person ("מעביר", "נציג"): the agent's words say them
    /// only with a transfer. A live call heard "רגע, מעביר למוקדן שיתקן את ההזמנה" and then
    /// the read-back, with no one transferred.
    #[serde(default)]
    pub transfer: Vec<String>,
    /// Curses and sexual words ("בן זונה", "בולבול"): nothing in words with one is taken, the
    /// first time the caller hears a calm line, the second the call ends. A live call sent
    /// "שיש לי בולבול גדול" to the drivers as the note.
    #[serde(default)]
    pub abuse: Vec<String>,
    /// A ride within one town ("נסיעה פנימית", "בתוך העיר", "לאותה עיר"): the other place is in
    /// the town of the one known.
    #[serde(default)]
    pub same_city: Vec<String>,
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
    /// Its keywords decide the task whatever the agent chose: "(כמה) עולה נסיעה מברקת
    /// לאלעד", "כמה" heard as "אמא", was taken for a booking and answered "איפה בברקת לאסוף?",
    /// and the caller hung up.
    #[serde(default)]
    pub decisive: bool,
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
    /// A place slot that a locality alone does not satisfy: it needs a street (or one of the
    /// business's known places). A taxi cannot pick up "אלעד".
    #[serde(default)]
    pub precise: bool,
    /// A place slot whose street is asked for once: a locality alone is held the first time
    /// ("לאיזה רחוב?") and taken when the caller gives it again ("לא יודע, ירושלים").
    #[serde(default)]
    pub street_once: bool,
    /// Regexes (Rust syntax) run against the match-normalized utterance. Each needs a
    /// named group `value`. Matched anywhere in the utterance, in any state.
    #[serde(default)]
    pub patterns: Vec<String>,
    /// Words a caller says with this detail when giving it unasked ("נוסעים", "אנחנו"). Set,
    /// a value the caller was not asked for and gave without one of them is not taken: a live
    /// call's "הנביאים שלוש" (the street) came back "אני מביאים שלוש", three passengers.
    #[serde(default)]
    pub cues: Vec<String>,
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
    /// Spoken when it is not known whether the action went through (it timed out after the
    /// request was sent): a person is asked to check, so it must not say "failed".
    #[serde(default)]
    pub on_unknown: Option<ResponseId>,
    /// Spoken when the pipeline has no action and all slots are filled.
    #[serde(default)]
    pub on_complete: Option<ResponseId>,
    /// What to do once the pipeline finishes.
    #[serde(default)]
    pub after: AfterPipeline,
    /// The questions come in the order of `slots`, always: a question the agent writes about
    /// any other detail is held back and the next one in the order asked instead. Details the
    /// caller gives out of order are taken; an optional question is asked once.
    #[serde(default)]
    pub strict_order: bool,
    /// Said after the task instead of "anything else?", when no task under way is resumed
    /// ("להזמין מונית?" after a price).
    #[serde(default)]
    pub offer: Option<ResponseId>,
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
    /// Take the value this slot has in another task of the call: a price question asked during
    /// a booking takes its pickup, destination and passengers ("כן, רק כמה זה עולה?" was
    /// answered with "מאיפה הנסיעה?" on a live call). A place of which only the city is known
    /// yet is taken as that city.
    #[serde(default)]
    pub from_slot: Option<SlotId>,
    /// Optional, but asked once before the read-back when never asked or given (the note
    /// for the driver).
    #[serde(default)]
    pub ask_before_confirm: bool,
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
fn default_cache_minutes() -> u64 {
    30
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
    /// A price-list bot asked in a chat (WhatsApp) on the business's behalf: the question from
    /// `query` ("מ {from} ל{to}", the cities of the task's places), the answer read as a
    /// price list and quoted for the ride (see `price_list`). Unavailable until the owner
    /// picks the account and the bot's chat on the settings page.
    PriceBot {
        query: String,
        /// Minutes the answer to the same question is used again without asking.
        #[serde(default = "default_cache_minutes")]
        cache_minutes: u64,
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
    /// How it is said: an audio tag for the voice model ("warmly", "cheerfully"), sent with the
    /// text to TTS and never said or shown.
    #[serde(default)]
    pub tone: Option<String>,
    /// Said alone: as the agent's phrase, none of its own words after it are said (the refusal
    /// of a question off the business must not be followed by the answer).
    #[serde(default)]
    pub alone: bool,
    /// Said once in a call: asked for again, this other response is said instead ("מה
    /// נשמע?" three times got "הכל טוב, תודה! במה אפשר לעזור?" three times).
    #[serde(default)]
    pub again: Option<ResponseId>,
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
    /// How long to wait after the caller asked to wait ("רגע"), and between the patient
    /// reprompts below.
    #[serde(default = "default_patient_after_ms")]
    pub patient_after_ms: u64,
    /// With details already given, reprompts said after `max_reprompts` before the call ends
    /// ("אני פה, אפשר לקחת את הזמן."): a caller looking for a house number is not hung up on.
    #[serde(default)]
    pub patient_reprompts: u32,
    #[serde(default)]
    pub patient_response: Option<ResponseId>,
}

fn default_patient_after_ms() -> u64 {
    15_000
}

impl Default for SilenceConfig {
    fn default() -> Self {
        Self {
            reprompt_after_ms: 7000,
            max_reprompts: 2,
            response: None,
            patient_after_ms: default_patient_after_ms(),
            patient_reprompts: 0,
            patient_response: None,
        }
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
    /// Greeting for a caller whose last ride is known ("אהלן דוד! שוב מאלעד לירושלים?"): may
    /// use `{customer_name}`, `{last_from_city}`, `{last_to_city}`.
    #[serde(default)]
    pub returning_greeting: Option<ResponseId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// Who the agent is and how it talks, in the business's language.
    pub persona: String,
    /// Business-specific rules the agent must follow (areas served, what it may promise).
    #[serde(default)]
    pub rules: Vec<String>,
    /// Responses offered to the agent as instant phrases: their fixed wording is
    /// pre-recorded, so a reply that uses one verbatim plays at once.
    #[serde(default)]
    pub phrases: Vec<ResponseId>,
    /// Said while the agent is still deciding. Unlike the rules' filler ("רגע, בודק"), it
    /// must fit any turn, small talk included: a plain "אממ...".
    #[serde(default)]
    pub thinking_filler: Option<ResponseId>,
    /// Filler delay on agent turns. The agent's first words take ~0.7 s, so a filler much
    /// earlier than that would talk over its answer.
    #[serde(default = "default_agent_filler_after")]
    pub filler_after_ms: u64,
    /// Ceiling for one agent decision; past it the rules decide the turn.
    #[serde(default = "default_agent_timeout")]
    pub timeout_ms: u64,
    /// The business's own words for the generic instructions (examples, the role).
    #[serde(default)]
    pub prompt: AgentPrompt,
    /// The agent reads the caller before it answers: each reply opens with `read` (what they
    /// mean, empty when they mean what they say) and `tone`. A few tokens a turn; off, the reply
    /// starts with its action as before.
    #[serde(default = "default_true")]
    pub reading: bool,
}

fn default_true() -> bool {
    true
}

/// What the generic agent prompt cannot know about a business: its examples, in its own
/// language. Everything is optional; an instruction without its example still stands.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentPrompt {
    /// The role the agent plays, as a person would ("dispatcher", "receptionist").
    #[serde(default)]
    pub role: Option<String>,
    /// A next question asked after taking a detail ("כמה נוסעים?").
    #[serde(default)]
    pub next_question: Option<String>,
    /// A complete place as a caller gives it ("דיזנגוף 50, תל אביב").
    #[serde(default)]
    pub place: Option<String>,
    /// Words after a preposition that name no place ("\"לשים מונית\" has no destination").
    #[serde(default)]
    pub not_a_place: Option<String>,
    /// A garbled utterance that is still enough to act on ("\"...רוצה ... מונית\" is enough
    /// to start a booking").
    #[serde(default)]
    pub garbled: Option<String>,
    /// A stuck question asked another way, with an example answer ("כמה אנשים נוסעים,
    /// למשל שניים?").
    #[serde(default)]
    pub stuck: Option<String>,
    /// How to address a caller whose gender is unknown, a man, a woman: for languages that
    /// inflect the second person. Without it, the agent is not told about address forms.
    #[serde(default)]
    pub address_forms: Option<AddressForms>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddressForms {
    /// Until the caller's gender is known: the instruction and its examples.
    pub neutral: String,
    pub masculine: String,
    pub feminine: String,
}

fn default_agent_filler_after() -> u64 {
    1100
}
fn default_agent_timeout() -> u64 {
    4000
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

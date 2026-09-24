//! Slot values and where they came from.

use serde::{Deserialize, Serialize};

use crate::time::TimeSpec;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SlotValue {
    Text {
        text: String,
    },
    Place {
        /// How it is spoken back ("נתב״ג", "רבי עקיבא 12").
        spoken: String,
        /// What the business action receives (a full address), when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        address: Option<String>,
        /// Set when resolved from the customer record ("home").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        customer_place: Option<String>,
    },
    Integer {
        value: i64,
    },
    Boolean {
        value: bool,
    },
    Time {
        time: TimeSpec,
    },
    Enum {
        value: String,
    },
}

impl SlotValue {
    /// The value as the caller would hear it read back.
    pub fn spoken(&self) -> String {
        match self {
            SlotValue::Text { text } => text.clone(),
            SlotValue::Place { spoken, .. } => spoken.clone(),
            SlotValue::Integer { value } => value.to_string(),
            SlotValue::Boolean { value } => if *value { "כן" } else { "לא" }.into(),
            SlotValue::Time { time } => time.spoken(),
            SlotValue::Enum { value } => value.clone(),
        }
    }

    /// The value as a business action receives it.
    pub fn to_action_json(&self) -> serde_json::Value {
        use serde_json::json;
        match self {
            SlotValue::Text { text } => json!(text),
            SlotValue::Place { spoken, address, customer_place } => json!({
                "spoken": spoken,
                "address": address.clone().unwrap_or_else(|| spoken.clone()),
                "customer_place": customer_place,
            }),
            SlotValue::Integer { value } => json!(value),
            SlotValue::Boolean { value } => json!(value),
            SlotValue::Time { time } => time.to_json(),
            SlotValue::Enum { value } => json!(value),
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            SlotValue::Integer { value } => Some(*value as f64),
            _ => None,
        }
    }

    /// Loose equality against a config JSON value (used by rules).
    pub fn matches_json(&self, other: &serde_json::Value) -> bool {
        match (self, other) {
            (SlotValue::Integer { value }, serde_json::Value::Number(n)) => n.as_i64() == Some(*value),
            (SlotValue::Boolean { value }, serde_json::Value::Bool(b)) => value == b,
            (SlotValue::Enum { value }, serde_json::Value::String(s)) => value == s,
            (SlotValue::Text { text }, serde_json::Value::String(s)) => text == s,
            (SlotValue::Place { spoken, .. }, serde_json::Value::String(s)) => spoken == s,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// Deterministic fast path.
    Rules,
    /// LLM structured extraction.
    Llm,
    /// Pipeline default.
    Default,
    /// Customer record.
    Customer,
    /// Business rule.
    Rule,
    /// The caller confirmed it explicitly.
    Confirmed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlotFill {
    pub slot: String,
    pub value: SlotValue,
    pub confidence: f32,
    pub provenance: Provenance,
}

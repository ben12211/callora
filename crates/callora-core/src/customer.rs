//! What the business already knows about the caller.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A customer record, as returned by the business's `customer_lookup` action. Only the
/// fields the runtime understands are typed; everything else rides along in `data` and is
/// available to response templates as `{customer.<field>}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Customer {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// Saved places keyed by name ("home", "work", "usual").
    #[serde(default)]
    pub places: BTreeMap<String, CustomerPlace>,
    /// Anything else the business returned.
    #[serde(default, flatten)]
    pub data: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomerPlace {
    /// How to say it back ("הבית", "רבי עקיבא 12").
    pub spoken: String,
    #[serde(default)]
    pub address: Option<String>,
}

impl Customer {
    pub fn from_json(value: &serde_json::Value) -> Option<Self> {
        if value.is_null() {
            return None;
        }
        let customer = value.get("customer").unwrap_or(value);
        serde_json::from_value(customer.clone()).ok()
    }
}

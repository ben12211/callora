//! Order cards: everything a completed task needs, gathered at the end of the call for
//! whoever acts on it (the dispatcher, the owner's page, a dispatch system). For a ride:
//! the passenger's name and phone, pickup and destination (as said and as the official
//! address), passengers, time and the note for the driver.

use serde_json::{json, Value};

use crate::business::Business;
use crate::state::CallState;
use crate::values::SlotValue;

/// One card per task completed successfully in the call, in the order they were done.
pub fn order_cards(b: &Business, state: &CallState) -> Vec<Value> {
    state
        .completed
        .iter()
        .filter(|run| run.outcome == "success")
        .filter_map(|run| {
            let pipeline = b.pipeline(&run.pipeline)?;
            let mut details = Vec::new();
            let mut line = vec![pipeline.description.clone()];
            if let Some(phone) = &state.caller_phone {
                line.push(format!("טלפון: {phone}"));
            }
            for ps in &pipeline.slots {
                let Some(value) = run.slots.get(&ps.slot) else { continue };
                let label = b.config.slots.get(&ps.slot).map_or(ps.slot.as_str(), |s| s.description.as_str());
                let spoken = value.spoken();
                // "אין" to the note question leaves nothing to show.
                if spoken.trim().is_empty() {
                    continue;
                }
                let address = match value {
                    SlotValue::Place { address: Some(a), .. } if *a != spoken => Some(a.clone()),
                    _ => None,
                };
                line.push(format!("{label}: {}", address.as_deref().unwrap_or(&spoken)));
                details.push(json!({ "field": ps.slot, "label": label, "value": spoken, "address": address }));
            }
            Some(json!({
                "task": pipeline.description,
                "pipeline": run.pipeline,
                "phone": state.caller_phone,
                "details": details,
                "result": run.result,
                "summary": line.join(" | "),
            }))
        })
        .collect()
}

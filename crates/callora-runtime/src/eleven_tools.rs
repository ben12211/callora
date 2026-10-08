//! The tools an ElevenLabs agent calls while it talks to a caller: `create_ride` and
//! `get_price`. They run the same business actions as Callora's own agent (the dispatch
//! backend, the price bot), put the ride on the orders page and send it to WhatsApp and
//! Telegram like any other order, so the owner sees no difference in where a ride comes from.
//!
//! The endpoint is open only with the token the settings page shows the owner: an HMAC of the
//! server's own secret, so nothing new has to be configured in the deployment.

use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;

use callora_core::business::{is_e164, Business};
use callora_core::gazetteer::Lookup;
use callora_core::time::TimeSpec;
use callora_core::values::SlotValue;

use crate::ports::{CallInfo, CallRecord, Usage};
use crate::server::AppState;

pub const TOOLS_PATH: &str = "/webhooks/elevenlabs/tools";

/// A ride told twice in one conversation (a retried tool call) is sent once.
const DUPLICATE_WINDOW: Duration = Duration::from_secs(30 * 60);

/// What the ElevenLabs tools send in the `x-callora-tools-token` header.
pub fn tools_token(secret: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key");
    mac.update(b"callora-elevenlabs-tools-v1");
    hex::encode(&mac.finalize().into_bytes()[..16])
}

fn text(body: &Value, key: &str) -> String {
    match &body[key] {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

/// A place as the caller said it: checked against the official lists when there are some, and
/// kept as said, marked for the driver, when it is not on them ("כניסה לעיר, ביתר עילית").
fn place(s: &AppState, said: &str) -> SlotValue {
    if let Some(Lookup::Found(a)) = s.services.gazetteer.as_ref().map(|g| g.resolve(said)) {
        return SlotValue::Place { spoken: a.spoken(), address: Some(a.official()), customer_place: None };
    }
    SlotValue::Place {
        spoken: said.to_string(),
        address: Some(format!("{said} (מקום לא מאומת: לתאם עם הנוסע)")),
        customer_place: None,
    }
}

fn fail(message: &str) -> Value {
    json!({ "ok": false, "message": message })
}

/// Runs a tool. Always an answer the agent can speak from: an error is `ok: false` with a
/// sentence, never a status the model would have to guess about.
pub async fn run(s: &AppState, tool: &str, body: &Value) -> Value {
    let Some(business) = s.registry.all().find(|b| b.config.actions.contains_key("create_ride")) else {
        return fail("אין עסק מוגדר");
    };
    match tool {
        "create-ride" | "create_ride" => create_ride(s, business, body).await,
        "get-price" | "get_price" => get_price(s, business, body).await,
        _ => fail("כלי לא מוכר"),
    }
}

async fn create_ride(s: &AppState, business: &Business, body: &Value) -> Value {
    let (pickup, destination) = (text(body, "pickup"), text(body, "destination"));
    if pickup.is_empty() || destination.is_empty() {
        return fail("חסר איסוף או יעד");
    }
    let passengers: i64 = text(body, "passengers").parse().unwrap_or(0);
    if !(1..=12).contains(&passengers) {
        return fail("מספר הנוסעים לא ברור");
    }
    let name = text(body, "customer_name");
    let notes = text(body, "notes");
    let phone = Some(text(body, "caller_number")).filter(|p| is_e164(p));
    let conversation = text(body, "conversation_id");

    if !conversation.is_empty() {
        let mut seen = s.tool_rides.lock();
        seen.retain(|_, at| at.elapsed() < DUPLICATE_WINDOW);
        if seen.insert(conversation.clone(), Instant::now()).is_some() {
            return json!({ "ok": true, "duplicate": true, "message": "הנסיעה כבר נשלחה" });
        }
    }

    let call_id = uuid::Uuid::new_v4();
    let info = CallInfo {
        call_id,
        call_sid: if conversation.is_empty() { format!("el_{call_id}") } else { format!("el_{conversation}") },
        business_id: business.config.id.clone(),
        from: phone.clone(),
        to: business.phone_numbers.first().cloned().unwrap_or_default(),
    };
    s.services.store.record(CallRecord::Started { info: info.clone() });

    let mut slots: Vec<(&str, SlotValue)> = vec![
        ("pickup", place(s, &pickup)),
        ("destination", place(s, &destination)),
        ("passengers", SlotValue::Integer { value: passengers }),
    ];
    if !name.is_empty() {
        slots.push(("customer_name", SlotValue::Text { text: name }));
    }
    if !notes.is_empty() {
        slots.push(("notes", SlotValue::Text { text: notes }));
    }
    let mut action_slots: serde_json::Map<String, Value> =
        slots.iter().map(|(k, v)| ((*k).to_string(), v.to_action_json())).collect();
    action_slots.insert("pickup_time".into(), TimeSpec::Now.to_json());
    let input = json!({
        "run_id": 1,
        "pipeline": "book_ride",
        "intent": "book_ride",
        "slots": action_slots,
        "customer": null,
        "caller_phone": phone,
    });

    let started = Instant::now();
    let result = s.services.actions.run(business, "create_ride", input.clone(), &info).await;
    let (ok, reply, verify) = match &result {
        Ok(v) => (true, v.clone(), false),
        Err(f) if f.outcome_unknown => (false, json!({ "error": f.error }), true),
        Err(f) => (false, json!({ "error": f.error }), false),
    };
    s.services.store.record(CallRecord::Action {
        call_id,
        action: "create_ride".into(),
        input,
        result: reply.clone(),
        ok,
        latency_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    });
    if ok || verify {
        s.services.store.record(CallRecord::Order {
            call_id,
            card: order_card(business, phone.as_deref(), &slots, &reply, verify),
        });
    }
    s.services.store.record(CallRecord::Ended {
        call_id,
        outcome: if ok {
            "success"
        } else if verify {
            "unknown"
        } else {
            "failed"
        }
        .into(),
        state: json!({ "source": "elevenlabs" }),
        usage: Usage::default(),
    });
    if !ok {
        // A ride that may have gone through is checked by a person; the agent says so, and
        // does not send it again.
        s.tool_rides.lock().remove(&conversation).filter(|_| !verify);
        tracing::warn!(error = %reply["error"], unknown = verify, "create_ride from the ElevenLabs agent did not complete");
        return if verify {
            json!({ "ok": false, "unknown": true, "message": "לא ברור אם ההזמנה נקלטה, מוקדן יבדוק" })
        } else {
            fail("לא הצלחתי לשלוח את ההזמנה")
        };
    }
    json!({ "ok": true, "message": "הנסיעה נשלחה" })
}

/// The order's card, the same shape the call's own orders have (`callora_core::orders`).
fn order_card(b: &Business, phone: Option<&str>, slots: &[(&str, SlotValue)], result: &Value, verify: bool) -> Value {
    let description = b.pipeline("book_ride").map_or("הזמנת מונית", |p| p.description.as_str()).to_string();
    let mut line = vec![description.clone()];
    if verify {
        line.insert(0, "לבדוק: לא ידוע אם נקלט".to_string());
    }
    if let Some(phone) = phone {
        line.push(format!("טלפון: {phone}"));
    }
    let mut details = Vec::new();
    for (slot, value) in slots {
        let label = b.config.slots.get(*slot).map_or(*slot, |s| s.description.as_str());
        let spoken = value.spoken();
        let address = match value {
            SlotValue::Place { address: Some(a), .. } if *a != spoken => Some(a.clone()),
            _ => None,
        };
        line.push(format!("{label}: {}", address.as_deref().unwrap_or(&spoken)));
        details.push(json!({ "field": slot, "label": label, "value": spoken, "address": address }));
    }
    json!({
        "task": description,
        "pipeline": "book_ride",
        "phone": phone,
        "details": details,
        "result": result,
        "verify": verify,
        "summary": line.join(" | "),
    })
}

async fn get_price(s: &AppState, business: &Business, body: &Value) -> Value {
    let (from, to) = (text(body, "price_from"), text(body, "price_to"));
    if from.is_empty() || to.is_empty() {
        return fail("חסרה עיר");
    }
    let mut slots = serde_json::Map::new();
    for (slot, city) in [("price_from", &from), ("price_to", &to)] {
        slots.insert(slot.into(), json!({ "spoken": city, "address": city }));
    }
    if let Ok(n) = text(body, "passengers").parse::<u32>() {
        slots.insert("passengers".into(), json!(n));
    }
    if body["round_trip"].as_bool() == Some(true) {
        slots.insert("round_trip".into(), json!(true));
    }
    let id = uuid::Uuid::new_v4();
    let info = CallInfo {
        call_id: id,
        call_sid: format!("el_price_{id}"),
        business_id: business.config.id.clone(),
        from: None,
        to: String::new(),
    };
    let input = json!({ "run_id": 1, "pipeline": "price_question", "intent": "price_question", "slots": slots });
    match s.services.actions.run(business, "estimate_price", input, &info).await {
        Ok(quote) => match price_sentence(business, &quote) {
            Some(message) => json!({ "ok": true, "message": message }),
            None => fail("אני לא מצליח לבדוק את המחיר כרגע"),
        },
        Err(e) => {
            tracing::warn!(error = %e.error, "get_price from the ElevenLabs agent failed");
            fail("אני לא מצליח לבדוק את המחיר כרגע")
        }
    }
}

/// The business's own answer to the quote, in words: "זה מאה עשרים שקלים."
fn price_sentence(b: &Business, quote: &Value) -> Option<String> {
    let id = quote["response"].as_str().unwrap_or("price_answer");
    let mut out = b.config.responses.get(id)?.variants.first()?.clone();
    for (key, value) in quote.as_object()? {
        let value = match value {
            Value::Number(n) => n.to_string(),
            Value::String(t) => t.clone(),
            _ => continue,
        };
        out = out.replace(&format!("{{{key}}}"), &value);
    }
    (!out.contains('{')).then(|| callora_core::speech::normalize_hebrew(&out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_is_stable_and_depends_on_the_secret() {
        assert_eq!(tools_token("a"), tools_token("a"));
        assert_ne!(tools_token("a"), tools_token("b"));
        assert_eq!(tools_token("a").len(), 32);
    }

    #[test]
    fn text_reads_strings_and_numbers() {
        let body = json!({ "a": " x ", "n": 3, "z": null });
        assert_eq!(text(&body, "a"), "x");
        assert_eq!(text(&body, "n"), "3");
        assert_eq!(text(&body, "z"), "");
        assert_eq!(text(&body, "missing"), "");
    }
}

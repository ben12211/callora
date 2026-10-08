//! The tools an ElevenLabs agent calls while it talks to a caller: `create_ride` and
//! `get_price`. They run the same business actions as Callora's own agent (the dispatch
//! backend, the price bot), put the ride on the orders page and send it to WhatsApp and
//! Telegram like any other order, so the owner sees no difference in where a ride comes from.
//!
//! The endpoint is open only with the token the settings page shows the owner: an HMAC of the
//! server's own secret, so nothing new has to be configured in the deployment.

use std::sync::Arc;
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

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

/// The id of the call row for an ElevenLabs-handled phone call: the same from its Twilio call
/// SID wherever it is needed (the webhook that hands the call over, a tool, the transcript), so
/// they all write to one call on the calls page.
pub fn eleven_call_id(call_sid: &str) -> uuid::Uuid {
    let digest = Sha256::digest(format!("callora-elevenlabs-call:{call_sid}").as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes)
}

/// Whether the conversation a tool call names is a real, live one of the agent the owner chose,
/// according to ElevenLabs itself. This is what lets the tools work with no shared secret to set up:
/// `conversation_id` comes from the platform (`system__conversation_id`, which the model cannot
/// change), is unguessable, and exists only while a caller is on the line. A positive answer is kept
/// for the length of a call.
pub async fn conversation_is_ours(s: &AppState, body: &Value) -> bool {
    let (conversation, call_sid) = (text(body, "conversation_id"), text(body, "call_sid"));
    let Some(agents) = s.eleven_agents.clone() else { return false };
    let agent_id = s.services.settings.call_mode().agent_id;
    if conversation.is_empty() || agent_id.trim().is_empty() {
        return false;
    }
    {
        let mut known = s.verified_conversations.lock();
        known.retain(|_, at| at.elapsed() < DUPLICATE_WINDOW);
        if known.contains_key(&conversation) {
            return true;
        }
    }
    let Ok(found) = agents.conversation(&conversation).await else { return false };
    let live = matches!(found["status"].as_str(), Some("in-progress" | "processing" | "initiated"));
    let ours = found["agent_id"].as_str() == Some(agent_id.trim());
    // When both say which phone call this is, it must be the same one.
    let same_call = match (found["metadata"]["phone_call"]["call_sid"].as_str(), call_sid.as_str()) {
        (Some(theirs), mine) if !mine.is_empty() => theirs == mine,
        _ => true,
    };
    if live && ours && same_call {
        s.verified_conversations.lock().insert(conversation, Instant::now());
        return true;
    }
    false
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
        "transfer-to-desk" | "transfer_to_desk" => transfer_to_desk(s, business, body).await,
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

    // On a phone call the tool names the call (`system__call_sid`): the ride joins that call's row.
    let twilio_sid = Some(text(body, "call_sid")).filter(|sid| sid.starts_with("CA"));
    let call_id = twilio_sid.as_deref().map_or_else(uuid::Uuid::new_v4, eleven_call_id);
    let info = CallInfo {
        call_id,
        call_sid: match (&twilio_sid, conversation.is_empty()) {
            (Some(sid), _) => sid.clone(),
            (None, true) => format!("el_{call_id}"),
            (None, false) => format!("el_{conversation}"),
        },
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
    // A phone call ends with its transcript (see `pull_transcript`); one with no phone call ends here.
    if twilio_sid.is_none() {
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
    }
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

/// Hands the caller to the dispatch desk the way Callora's own agent does: the callers' call is
/// moved to the desk's conference, the desk numbers ring, and the order card's WhatsApp
/// targets are told. Answers at once so the agent can say it; the move follows two seconds later.
async fn transfer_to_desk(s: &AppState, business: &Business, body: &Value) -> Value {
    let call_sid = text(body, "call_sid");
    if !call_sid.starts_with("CA") {
        return fail("אי אפשר להעביר: חסר מזהה שיחה");
    }
    let (Some(desk), settings) = (s.services.desk.clone(), s.services.settings.desk(business)) else {
        return fail("אין מוקדן מוגדר");
    };
    if settings.numbers.is_empty() {
        return fail("אין מוקדן מוגדר");
    }
    let call_id = eleven_call_id(&call_sid);
    let info = CallInfo {
        call_id,
        call_sid: call_sid.clone(),
        business_id: business.config.id.clone(),
        from: Some(text(body, "caller_number")).filter(|p| is_e164(p)),
        to: business.phone_numbers.first().cloned().unwrap_or_default(),
    };
    s.services.store.record(CallRecord::Started { info: info.clone() });
    let said: String = text(body, "summary").chars().take(300).collect();
    let reason = Some(text(body, "reason")).filter(|r| !r.is_empty()).unwrap_or_else(|| "הסוכן ביקש מוקדן".into());
    let summary = callora_core::engine::HandoffSummary {
        reason,
        business_id: business.config.id.clone(),
        intent: None,
        pipeline: None,
        slots: Vec::new(),
        customer_name: None,
        recent: Vec::new(),
        text: said,
    };
    s.services.store.record(CallRecord::Handoff { call_id, summary: summary.clone() });
    s.services.metrics.handoffs_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let language = business.config.language.clone();
    let unavailable = business
        .response(&business.config.handoff.unavailable_response)
        .and_then(|r| r.variants.first().cloned())
        .unwrap_or_default();
    let telephony = s.services.telephony.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if !desk.transfer(&info, &summary, &settings, &language, &unavailable).await {
            tracing::error!(call = %info.call_sid, "the call could not be moved to the desk; hanging up");
            let _ = telephony.hangup(&info.call_sid).await;
        }
    });
    json!({ "ok": true, "message": "מעביר למוקדן" })
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

/// The transcript of a finished ElevenLabs call, as the records the calls page reads.
pub fn transcript_records(call_id: uuid::Uuid, conversation: &Value) -> Vec<CallRecord> {
    let mut records: Vec<CallRecord> = conversation["transcript"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|turn| {
            let message = turn["message"].as_str().unwrap_or("").trim();
            // A turn with no words but a tool call is shown as the call: "⚙ create_ride (שגיאה)".
            let tools: Vec<String> = turn["tool_calls"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| c["tool_name"].as_str())
                .map(|name| {
                    let failed = turn["tool_results"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|r| r["tool_name"].as_str() == Some(name) && r["is_error"].as_bool() == Some(true));
                    if failed {
                        format!("{name} (שגיאה)")
                    } else {
                        name.to_string()
                    }
                })
                .collect();
            let text = match (message.is_empty(), tools.is_empty()) {
                (true, true) => return None,
                (true, false) => format!("⚙ {}", tools.join(", ")),
                (false, _) => message.to_string(),
            };
            let speaker = if turn["role"].as_str() == Some("user") { "caller" } else { "agent" };
            Some(CallRecord::Turn {
                call_id,
                speaker: speaker.into(),
                text,
                detail: json!({
                    "source": "elevenlabs",
                    "at": turn["time_in_call_secs"],
                    "tools": turn["tool_calls"],
                }),
            })
        })
        .collect();
    let analysis = &conversation["analysis"];
    let outcome = match analysis["call_successful"].as_str() {
        Some("success") => "success",
        Some("failure") => "failed",
        _ => "completed",
    };
    records.push(CallRecord::Ended {
        call_id,
        outcome: outcome.into(),
        state: json!({
            "source": "elevenlabs",
            "conversation_id": conversation["conversation_id"],
            "summary": analysis["transcript_summary"],
        }),
        usage: Usage::default(),
    });
    records
}

/// After an ElevenLabs call ends: finds its conversation (the one whose phone call is this Twilio
/// call) and stores the transcript on the call's row. ElevenLabs needs a few seconds to finish it.
pub async fn pull_transcript(s: Arc<AppState>, call_sid: String, started_unix: i64) {
    let Some(agents) = s.eleven_agents.clone() else { return };
    let agent_id = s.services.settings.call_mode().agent_id;
    for attempt in 0..6u64 {
        tokio::time::sleep(Duration::from_secs(if attempt == 0 { 6 } else { 10 })).await;
        let found = async {
            for (id, _) in agents.conversations(agent_id.trim(), started_unix - 120).await? {
                let conversation = agents.conversation(&id).await?;
                if conversation["metadata"]["phone_call"]["call_sid"].as_str() == Some(call_sid.as_str()) {
                    return anyhow::Ok(Some(conversation));
                }
            }
            anyhow::Ok(None)
        }
        .await;
        match found {
            Ok(Some(conversation)) => {
                let call_id = eleven_call_id(&call_sid);
                for record in transcript_records(call_id, &conversation) {
                    s.services.store.record(record);
                }
                tracing::info!(call = %call_sid, "the ElevenLabs transcript is on the calls page");
                return;
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(call = %call_sid, error = %e, "ElevenLabs transcript not read yet"),
        }
    }
    tracing::warn!(call = %call_sid, "no ElevenLabs transcript was found for the call");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_phone_call_has_one_id_wherever_it_is_needed() {
        assert_eq!(eleven_call_id("CA123"), eleven_call_id("CA123"));
        assert_ne!(eleven_call_id("CA123"), eleven_call_id("CA124"));
    }

    #[test]
    fn a_transcript_becomes_the_calls_turns_and_its_end() {
        let id = eleven_call_id("CA1");
        let conversation = json!({
            "conversation_id": "conv_1",
            "transcript": [
                { "role": "agent", "message": "אהלן", "time_in_call_secs": 0, "tool_calls": [] },
                { "role": "user", "message": " צריך מונית ", "time_in_call_secs": 4, "tool_calls": [] },
                { "role": "agent", "message": "", "time_in_call_secs": 6, "tool_calls": [{ "tool_name": "create_ride" }],
                  "tool_results": [{ "tool_name": "create_ride", "is_error": true }] },
                { "role": "agent", "message": "", "time_in_call_secs": 7, "tool_calls": [] },
            ],
            "analysis": { "call_successful": "success", "transcript_summary": "הוזמנה מונית" },
        });
        let records = transcript_records(id, &conversation);
        assert_eq!(records.len(), 4, "two turns with words, a tool call and the end: {records:?}");
        assert!(matches!(&records[0], CallRecord::Turn { speaker, text, .. } if speaker == "agent" && text == "אהלן"));
        assert!(
            matches!(&records[1], CallRecord::Turn { speaker, text, .. } if speaker == "caller" && text == "צריך מונית")
        );
        assert!(matches!(&records[2], CallRecord::Turn { text, .. } if text == "⚙ create_ride (שגיאה)"));
        assert!(matches!(&records[3], CallRecord::Ended { outcome, .. } if outcome == "success"));
    }

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

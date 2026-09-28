//! What the calls page shows beyond the raw history: the numbers that say whether the agent
//! is doing its job, and a call turned into an eval case (see `evaluation/README.md`).

use serde_json::{json, Value};

use crate::ports::Usage;
use crate::pricing::{cost, Prices};
use crate::store::CallFacts;

/// The numbers for a set of calls. The one that matters most: the share of calls that ended
/// with the task done and no person involved.
pub fn stats(facts: &[CallFacts], prices: &Prices) -> Value {
    let calls = facts.len();
    let handed_off = facts.iter().filter(|f| f.outcome.as_deref() == Some("HandedOff")).count();
    let done_alone = facts.iter().filter(|f| f.orders > 0 && f.outcome.as_deref() != Some("HandedOff")).count();
    let nothing_done = facts.iter().filter(|f| f.orders == 0 && f.outcome.as_deref() != Some("HandedOff")).count();
    let unverified: i64 = facts.iter().map(|f| f.unverified).sum();
    let reviewed = |v: &str| facts.iter().filter(|f| f.verdict.as_deref() == Some(v)).count();
    let durations: Vec<i32> = facts.iter().filter_map(|f| f.duration_seconds).collect();
    let mut tokens = Usage::default();
    let mut dollars = 0.0;
    let mut metered = 0usize;
    let mut priced = 0usize;
    for u in facts.iter().filter_map(|f| f.usage.as_ref()) {
        tokens.add(u);
        metered += 1;
        if let Some(c) = cost(u, prices) {
            dollars += c;
            priced += 1;
        }
    }
    tokens.model.clear();
    // How fast a task gets done: the median over the calls that did one.
    let median = |mut v: Vec<f64>| {
        v.sort_by(f64::total_cmp);
        let n = v.len();
        (n > 0).then(|| if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 })
    };
    let booking_seconds = median(facts.iter().filter_map(|f| f.booking_seconds).collect());
    let booking_turns = median(facts.iter().filter_map(|f| f.booking_turns.map(|n| n as f64)).collect());
    let share = |n: usize| if calls == 0 { None } else { Some(n as f64 / calls as f64) };
    json!({
        "calls": calls,
        "done_without_a_person": done_alone,
        "done_without_a_person_share": share(done_alone),
        "handed_off": handed_off,
        "handed_off_share": share(handed_off),
        "nothing_done": nothing_done,
        "to_verify": unverified,
        "reviewed_good": reviewed("good"),
        "reviewed_bad": reviewed("bad"),
        "booking_seconds_median": booking_seconds,
        "booking_turns_median": booking_turns,
        "avg_duration_seconds": (!durations.is_empty())
            .then(|| durations.iter().map(|d| f64::from(*d)).sum::<f64>() / durations.len() as f64),
        "tokens": tokens,
        // Only when every metered call has a price: a partial sum would look cheaper than it is.
        "cost": (metered > 0 && priced == metered).then_some(dollars),
        "cost_per_call": (metered > 0 && priced == metered).then(|| dollars / metered as f64),
    })
}

/// A call from the history (as `store::get_call` returns it) as an eval case. Each caller
/// turn the agent decided keeps its recorded decision as `scripted`, so the case replays
/// the call as it went; turns the engine answered alone (the fast lane) replay by
/// themselves. The operator then keeps the turn that went wrong, removes its `scripted`,
/// and writes its `expect`: until then the case fails `callora eval --check` on purpose
/// ("every turn is scripted").
pub fn eval_case(call: &Value) -> Value {
    let sid = call["call_sid"].as_str().unwrap_or("call");
    let started = call["started_at"].as_str().unwrap_or("");
    let mut note = format!("live call {sid} {}", started.get(..10).unwrap_or(started));
    if let Some(n) = call["review"]["note"].as_str().filter(|n| !n.is_empty()) {
        note.push_str(": ");
        note.push_str(n);
    }
    let mut turns = Vec::new();
    for t in call["turns"].as_array().into_iter().flatten().filter(|t| t["speaker"] == "caller") {
        let mut turn = json!({ "caller": t["text"] });
        let detail = &t["detail"];
        if let Some(second) = detail["second_hearing"].as_str().filter(|s| !s.is_empty()) {
            turn["second_hearing"] = json!(second);
        }
        if detail["route"] == "agent" {
            if let Some(reply) = detail["reply"].as_object() {
                let keep = ["action", "task", "fields", "asks", "phrase", "say"];
                let scripted: serde_json::Map<String, Value> = reply
                    .iter()
                    .filter(|(k, _)| keep.contains(&k.as_str()))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                turn["scripted"] = Value::Object(scripted);
            }
        }
        turns.push(turn);
    }
    let id: String = format!("call_{sid}")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .collect();
    json!([{
        "id": id,
        "note": note,
        "business": call["business_id"],
        "turns": turns,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::parse_prices;

    fn facts(outcome: &str, orders: i64, usage: Option<Usage>) -> CallFacts {
        CallFacts { outcome: Some(outcome.into()), duration_seconds: Some(60), orders, usage, ..CallFacts::default() }
    }

    #[test]
    fn the_numbers_of_a_day() {
        let u = |m: &str| Some(Usage { model: m.into(), input: 10_000, cached: 8_000, output: 200 });
        let day = [
            facts("AgentHungUp", 1, u("gpt-6-sol")),
            facts("CallerHungUp", 1, u("gpt-6-sol")),
            facts("HandedOff", 0, u("gpt-6-sol")),
            facts("CallerHungUp", 0, None),
        ];
        let prices = parse_prices("gpt-6-sol=2/0.2/10");
        let s = stats(&day, &prices);
        assert!(s["booking_seconds_median"].is_null(), "no call says when its task was done");
        assert_eq!(s["calls"], 4);
        assert_eq!(s["done_without_a_person"], 2);
        assert_eq!(s["done_without_a_person_share"], 0.5);
        assert_eq!(s["handed_off"], 1);
        assert_eq!(s["nothing_done"], 1);
        assert_eq!(s["tokens"]["input"], 30_000);
        let per_call = s["cost_per_call"].as_f64().unwrap();
        assert!((per_call - (2000.0 * 2.0 + 8000.0 * 0.2 + 200.0 * 10.0) / 1e6).abs() < 1e-12, "{per_call}");

        // A model without a price makes the cost unknown, not smaller.
        let s = stats(&[facts("AgentHungUp", 1, u("gpt-6-sol")), facts("AgentHungUp", 1, u("other"))], &prices);
        assert!(s["cost"].is_null());
        assert!(stats(&[], &prices)["done_without_a_person_share"].is_null());
    }

    #[test]
    fn how_fast_a_ride_gets_booked() {
        let booked = |secs: f64, turns: i64| CallFacts {
            orders: 1,
            booking_seconds: Some(secs),
            booking_turns: Some(turns),
            ..CallFacts::default()
        };
        let s = stats(
            &[booked(40.0, 4), booked(70.0, 7), booked(55.0, 5), facts("CallerHungUp", 0, None)],
            &Prices::default(),
        );
        assert_eq!(s["booking_seconds_median"], 55.0);
        assert_eq!(s["booking_turns_median"], 5.0);
        let s = stats(&[booked(40.0, 4), booked(60.0, 6)], &Prices::default());
        assert_eq!(s["booking_seconds_median"], 50.0);
    }

    #[test]
    fn a_call_becomes_an_eval_case_that_replays_it() {
        let call = json!({
            "call_sid": "CA12ab", "business_id": "taxi", "started_at": "2026-09-27T10:00:00Z",
            "review": { "verdict": "bad", "note": "booked מיתר" },
            "turns": [
                { "speaker": "agent", "text": "אהלן, איך אפשר לעזור?", "detail": {} },
                { "speaker": "caller", "text": "צריך מונית מביתר", "detail": {
                    "route": "agent", "decision_ms": 700, "second_hearing": "צריך מונית מביתר",
                    "reply": { "action": "none", "task": "book_ride", "fields": [{ "slot": "pickup", "value": "ביתר" }],
                               "phrase": "ask_pickup_street", "say": "" } } },
                { "speaker": "agent", "text": "מאיזה רחוב ומספר לאסוף?", "detail": {} },
                { "speaker": "caller", "text": "כן", "detail": { "transcript": "כן", "affirm": true } }
            ]
        });
        let case = eval_case(&call);
        let c = &case[0];
        assert_eq!(c["id"], "call_ca12ab");
        assert_eq!(c["note"], "live call CA12ab 2026-09-27: booked מיתר");
        assert_eq!(c["turns"].as_array().unwrap().len(), 2, "caller turns only");
        assert_eq!(c["turns"][0]["scripted"]["phrase"], "ask_pickup_street");
        assert_eq!(c["turns"][0]["second_hearing"], "צריך מונית מביתר");
        assert!(c["turns"][1].get("scripted").is_none(), "the fast lane replays by itself");
    }
}

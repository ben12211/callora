//! Call history against a real PostgreSQL. Runs when `TEST_DATABASE_URL` is set (always in
//! `./dev test` and CI); skipped otherwise.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use callora_core::engine::HandoffSummary;
use callora_runtime::ports::{CallInfo, CallRecord, CallStore, Usage};
use callora_runtime::store;
use serde_json::json;

#[tokio::test]
async fn records_round_trip_through_postgres() {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("TEST_DATABASE_URL not set; skipping");
        return;
    };
    let pool = store::connect(&url).await.expect("database reachable");
    store::migrate(&pool).await.expect("migrations apply");
    store::migrate(&pool).await.expect("and are idempotent");

    let pg = store::PgStore::spawn(pool.clone());
    let call_id = uuid::Uuid::new_v4();
    let sid = format!("CA{}", call_id.simple());
    pg.record(CallRecord::Started {
        info: CallInfo {
            call_id,
            call_sid: sid.clone(),
            business_id: "taxi".into(),
            from: Some("+972501111111".into()),
            to: "+972500000000".into(),
        },
    });
    pg.record(CallRecord::Turn {
        call_id,
        speaker: "caller".into(),
        text: "צריך מונית".into(),
        detail: json!({ "intent": "book_ride" }),
    });
    pg.record(CallRecord::Turn {
        call_id,
        speaker: "agent".into(),
        text: "מאיפה לאסוף אותך?".into(),
        detail: json!({}),
    });
    pg.record(CallRecord::Action {
        call_id,
        action: "create_ride".into(),
        input: json!({}),
        result: json!({ "eta_minutes": 4 }),
        ok: true,
        latency_ms: 812,
    });
    pg.record(CallRecord::Handoff {
        call_id,
        summary: HandoffSummary {
            reason: "caller_requested".into(),
            business_id: "taxi".into(),
            intent: Some("book_ride".into()),
            pipeline: Some("book_ride".into()),
            slots: vec![("יעד".into(), "נתב״ג".into())],
            customer_name: None,
            recent: vec![],
            text: "שיחה מועברת".into(),
        },
    });
    pg.record(CallRecord::Utterance { call_id, heard: "צריך מונית".into(), audio: vec![0xFF; 1600] });
    let usage = Usage { model: "gpt-6-sol".into(), input: 3000, cached: 2500, output: 60 };
    let meter = callora_runtime::ports::Meter { tts_chars: 120, tts_requests: 2, second_hearings: 1 };
    pg.record(CallRecord::Ended { call_id, outcome: "HandedOff".into(), state: json!({ "turns": 1 }), usage, meter });
    pg.record(CallRecord::Status { call_sid: sid, status: "completed".into(), duration_seconds: Some(42) });

    // The writer is asynchronous by design.
    let mut call = None;
    for _ in 0..50 {
        call = store::get_call(&pool, call_id).await.unwrap();
        if call
            .as_ref()
            .is_some_and(|c| c["outcome"] == "HandedOff" && c["handoffs"].as_array().is_some_and(|h| !h.is_empty()))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let call = call.expect("call stored");
    assert_eq!(call["turns"].as_array().unwrap().len(), 2);
    assert_eq!(call["turns"][0]["text"], "צריך מונית");
    assert_eq!(call["actions"][0]["latency_ms"], 812);
    assert_eq!(call["handoffs"][0]["reason"], "caller_requested");
    let listed = store::list_calls(&pool, Some("taxi"), 10, 0).await.unwrap();
    let row = listed.iter().find(|c| c["id"] == json!(call_id)).expect("listed");
    assert_eq!(row["usage"]["input"], 3000);
    assert_eq!(row["orders"], 0);
    // What it used, for the costs page.
    let (costs, _) = store::call_costs(&pool, Some("taxi"), 50).await.unwrap();
    let (_, used) = costs.iter().find(|(c, _)| c["id"] == json!(call_id)).expect("priced");
    assert_eq!(used.meter.as_ref().map(|m| m.tts_chars), Some(120));
    assert_eq!(used.seconds, Some(42.0));
    assert_eq!(call["utterances"][0]["heard"], "צריך מונית");

    // The owner's verdict, replaced by a later one; unknown calls are refused.
    assert!(store::set_review(&pool, call_id, "good", "").await.unwrap());
    assert!(store::set_review(&pool, call_id, "bad", "asked the street twice").await.unwrap());
    assert!(!store::set_review(&pool, uuid::Uuid::new_v4(), "bad", "").await.unwrap());
    let call = store::get_call(&pool, call_id).await.unwrap().unwrap();
    assert_eq!(call["review"], json!({ "verdict": "bad", "note": "asked the street twice" }));
    let facts = store::call_facts(&pool, Some("taxi"), 1).await.unwrap();
    assert!(facts.iter().any(|f| f.verdict.as_deref() == Some("bad")
        && f.outcome.as_deref() == Some("HandedOff")
        && f.booking_seconds.is_none()
        && f.booking_turns.is_none()));

    let daily = store::daily(&pool, Some("taxi"), 1).await.unwrap();
    assert!(daily.iter().any(|d| d["calls"].as_i64() >= Some(1) && d["handed_off"].as_i64() >= Some(1)), "{daily:?}");

    let id = call["utterances"][0]["id"].as_i64().unwrap();
    assert_eq!(store::utterance_audio(&pool, id).await.unwrap().map(|a| a.len()), Some(1600));
}

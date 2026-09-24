//! Call history against a real PostgreSQL. Runs when `TEST_DATABASE_URL` is set (always in
//! `./dev test` and CI); skipped otherwise.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use callora_core::engine::HandoffSummary;
use callora_runtime::ports::{CallInfo, CallRecord, CallStore};
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
    pg.record(CallRecord::Ended { call_id, outcome: "HandedOff".into(), state: json!({ "turns": 1 }) });
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
    assert!(listed.iter().any(|c| c["id"] == json!(call_id)));
}

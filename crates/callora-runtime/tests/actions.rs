//! Business actions over HTTP: the idempotency key, and which failures leave the outcome
//! unknown.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use parking_lot::Mutex;
use serde_json::{json, Value};

use callora_core::business::Business;
use callora_runtime::actions::ConfiguredActions;
use callora_runtime::ports::{ActionRunner, CallInfo};

const TAXI: &str = include_str!("../../../businesses/taxi.json");

fn call() -> CallInfo {
    CallInfo {
        call_id: uuid::Uuid::from_u128(7),
        call_sid: "CA7".into(),
        business_id: "taxi".into(),
        from: Some("+972501111111".into()),
        to: "+972500000000".into(),
    }
}

/// A dispatch endpoint that records the keys it gets and answers after `delay`.
async fn dispatch(delay: Duration) -> (String, Arc<Mutex<Vec<(String, String)>>>) {
    let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let log = seen.clone();
    let app = Router::new().route(
        "/dispatch",
        post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let log = log.clone();
            async move {
                let header = headers.get("idempotency-key").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                log.lock().push((header, body["idempotency_key"].as_str().unwrap_or("").to_string()));
                tokio::time::sleep(delay).await;
                Json(json!({ "ride_id": "R-1" }))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/dispatch"), seen)
}

fn runner(url: &str) -> ConfiguredActions {
    let env = HashMap::from([("TAXI_DISPATCH_URL".to_string(), url.to_string())]);
    ConfiguredActions::new(reqwest::Client::new(), env)
}

fn taxi() -> Business {
    Business::from_json(TAXI, "taxi.json", &|_| None).unwrap()
}

#[tokio::test]
async fn every_attempt_of_one_run_carries_the_same_key() {
    let (url, seen) = dispatch(Duration::ZERO).await;
    let actions = runner(&url);
    let b = taxi();
    let input = json!({ "run_id": 3, "slots": {} });
    actions.run(&b, "create_ride", input.clone(), &call()).await.unwrap();
    actions.run(&b, "create_ride", input, &call()).await.unwrap();
    actions.run(&b, "create_ride", json!({ "run_id": 4, "slots": {} }), &call()).await.unwrap();
    let seen = seen.lock();
    assert_eq!(seen[0].0, seen[0].1, "header and body agree");
    assert_eq!(seen[0].0, seen[1].0, "a retry is the same request");
    assert_ne!(seen[0].0, seen[2].0, "another run is another request");
    assert!(seen[0].0.starts_with(&call().call_id.to_string()));
}

#[tokio::test]
async fn a_timeout_after_sending_leaves_the_outcome_unknown() {
    // create_ride times out after 6 s; a slower backend may still book the ride.
    let (url, seen) = dispatch(Duration::from_secs(10)).await;
    let failure = runner(&url).run(&taxi(), "create_ride", json!({ "run_id": 1 }), &call()).await.unwrap_err();
    assert!(failure.outcome_unknown, "{failure}");
    assert_eq!(seen.lock().len(), 1, "the request did arrive");
}

#[tokio::test]
async fn a_backend_that_cannot_be_reached_has_not_booked_anything() {
    // Nothing listens on this port: the request never left.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/dispatch", listener.local_addr().unwrap());
    drop(listener);
    let failure = runner(&url).run(&taxi(), "create_ride", json!({ "run_id": 1 }), &call()).await.unwrap_err();
    assert!(!failure.outcome_unknown, "{failure}");
}

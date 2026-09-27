//! Executes business actions from their config: the first configured backend wins.
//!
//! An `http` backend is "configured" when its URL environment variable is set, so a
//! business can ship with a `mock` fallback and switch to its real system by setting one
//! variable. The runtime never knows what an action means; it posts the pipeline's slots
//! and hands the JSON reply to the response templates.
//!
//! Every request carries an idempotency key (the `Idempotency-Key` header and
//! `idempotency_key` in the body), the same for every attempt of one run of a task in one
//! call: a backend that honours it books a ride once even when a timed-out first attempt
//! did reach it. A failure after the request may have arrived (a timeout, a broken
//! connection) is reported as an unknown outcome, never as "failed".

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use callora_core::business::Business;
use callora_core::config::ActionBackend;
use callora_core::engine::ActionFailure;

use crate::ports::{ActionRunner, CallInfo};

pub struct ConfiguredActions {
    http: reqwest::Client,
    env: HashMap<String, String>,
}

impl ConfiguredActions {
    /// `env` is a snapshot of the process environment (injectable for tests).
    pub fn new(http: reqwest::Client, env: HashMap<String, String>) -> Self {
        Self { http, env }
    }

    fn var(&self, name: &str) -> Option<&str> {
        self.env.get(name).map(String::as_str).filter(|v| !v.trim().is_empty())
    }
}

/// The same for every attempt of one run of a task in one call, different for anything else.
pub fn idempotency_key(call: &CallInfo, action: &str, input: &Value) -> String {
    let run = input.get("run_id").and_then(Value::as_u64).unwrap_or(0);
    format!("{}:{action}:{run}", call.call_id)
}

/// Whether a failed request may still have reached the backend.
fn may_have_arrived(e: &reqwest::Error) -> bool {
    !e.is_connect() && !e.is_builder()
}

#[async_trait]
impl ActionRunner for ConfiguredActions {
    async fn run(
        &self,
        business: &Business,
        action: &str,
        input: Value,
        call: &CallInfo,
    ) -> Result<Value, ActionFailure> {
        let config = business
            .config
            .actions
            .get(action)
            .ok_or_else(|| ActionFailure::failed(format!("unknown action {action}")))?;
        let timeout = Duration::from_millis(config.timeout_ms);
        for backend in &config.backends {
            match backend {
                ActionBackend::Http { url_env, token_env } => {
                    let Some(url) = self.var(url_env) else { continue };
                    let key = idempotency_key(call, action, &input);
                    let body = json!({
                        "action": action,
                        "business": business.config.id,
                        "idempotency_key": key,
                        "call": { "id": call.call_id, "sid": call.call_sid, "from": call.from, "to": call.to },
                        "input": input,
                    });
                    let mut req = self.http.post(url).timeout(timeout).header("Idempotency-Key", &key).json(&body);
                    if let Some(token) = token_env.as_deref().and_then(|t| self.var(t)) {
                        req = req.bearer_auth(token);
                    }
                    let resp = req.send().await.map_err(|e| {
                        let message = format!("{action}: request failed: {e}");
                        if may_have_arrived(&e) {
                            ActionFailure::unknown(message)
                        } else {
                            ActionFailure::failed(message)
                        }
                    })?;
                    let status = resp.status();
                    // The backend answered, so its status is the outcome; a body cut off on
                    // a success leaves the outcome unknown.
                    let value: Value = match resp.json().await {
                        Ok(v) => v,
                        Err(e) if status.is_success() => {
                            return Err(ActionFailure::unknown(format!("{action}: unreadable reply ({status}): {e}")))
                        }
                        Err(_) => Value::Null,
                    };
                    if !status.is_success() {
                        return Err(ActionFailure::failed(format!(
                            "{action}: HTTP {status}: {}",
                            value.get("error").and_then(Value::as_str).unwrap_or("error")
                        )));
                    }
                    if let Some(err) = value.get("error").and_then(Value::as_str) {
                        return Err(ActionFailure::failed(format!("{action}: {err}")));
                    }
                    return Ok(value);
                }
                ActionBackend::Mock { result, latency_ms, error } => {
                    if *latency_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(*latency_ms).min(timeout)).await;
                    }
                    return match error {
                        Some(e) => Err(ActionFailure::failed(e.clone())),
                        None => Ok(result.clone()),
                    };
                }
            }
        }
        Err(ActionFailure::failed(format!("{action}: no backend is configured")))
    }
}

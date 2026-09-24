//! Executes business actions from their config: the first configured backend wins.
//!
//! An `http` backend is "configured" when its URL environment variable is set, so a
//! business can ship with a `mock` fallback and switch to its real system by setting one
//! variable. The runtime never knows what an action means; it posts the pipeline's slots
//! and hands the JSON reply to the response templates.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use callora_core::business::Business;
use callora_core::config::ActionBackend;

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

#[async_trait]
impl ActionRunner for ConfiguredActions {
    async fn run(&self, business: &Business, action: &str, input: Value, call: &CallInfo) -> Result<Value, String> {
        let config = business.config.actions.get(action).ok_or_else(|| format!("unknown action {action}"))?;
        let timeout = Duration::from_millis(config.timeout_ms);
        for backend in &config.backends {
            match backend {
                ActionBackend::Http { url_env, token_env } => {
                    let Some(url) = self.var(url_env) else { continue };
                    let body = json!({
                        "action": action,
                        "business": business.config.id,
                        "call": { "id": call.call_id, "sid": call.call_sid, "from": call.from, "to": call.to },
                        "input": input,
                    });
                    let mut req = self.http.post(url).timeout(timeout).json(&body);
                    if let Some(token) = token_env.as_deref().and_then(|t| self.var(t)) {
                        req = req.bearer_auth(token);
                    }
                    let resp = req.send().await.map_err(|e| format!("{action}: request failed: {e}"))?;
                    let status = resp.status();
                    let value: Value = resp.json().await.map_err(|e| format!("{action}: invalid JSON reply ({status}): {e}"))?;
                    if !status.is_success() {
                        return Err(format!("{action}: HTTP {status}: {}", value.get("error").and_then(Value::as_str).unwrap_or("error")));
                    }
                    if let Some(err) = value.get("error").and_then(Value::as_str) {
                        return Err(format!("{action}: {err}"));
                    }
                    return Ok(value);
                }
                ActionBackend::Mock { result, latency_ms, error } => {
                    if *latency_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(*latency_ms).min(timeout)).await;
                    }
                    return match error {
                        Some(e) => Err(e.clone()),
                        None => Ok(result.clone()),
                    };
                }
            }
        }
        Err(format!("{action}: no backend is configured"))
    }
}

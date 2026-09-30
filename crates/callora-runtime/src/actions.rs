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
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use callora_core::business::Business;
use callora_core::config::ActionBackend;
use callora_core::engine::ActionFailure;

use callora_core::price_list;

use crate::ports::{ActionRunner, CallInfo, ChatBot};

pub struct ConfiguredActions {
    http: reqwest::Client,
    env: HashMap<String, String>,
    chat_bot: Option<Arc<dyn ChatBot>>,
}

impl ConfiguredActions {
    /// `env` is a snapshot of the process environment (injectable for tests).
    pub fn new(http: reqwest::Client, env: HashMap<String, String>) -> Self {
        Self { http, env, chat_bot: None }
    }

    pub fn with_chat_bot(mut self, bot: Arc<dyn ChatBot>) -> Self {
        self.chat_bot = Some(bot);
        self
    }

    /// The price bot's question for the ride in `input` ("מ בני ברק לירושלים"), and the places
    /// as the caller and the system wrote them (a neighbourhood among them has its own prices).
    fn price_question(query: &str, input: &Value) -> Option<(String, Vec<String>)> {
        let slots = &input["slots"];
        let place = |names: [&str; 2]| {
            names.iter().find_map(|n| {
                let v = &slots[*n];
                let spoken = v["spoken"].as_str().or(v.as_str()).map(str::to_string);
                let address = v["address"].as_str().map(str::to_string);
                spoken.clone().or(address.clone()).map(|s| (s, address))
            })
        };
        let ((from, from_address), (to, to_address)) =
            (place(["price_from", "pickup"])?, place(["price_to", "destination"])?);
        let question = query.replace("{from}", &price_list::city_of(&from)).replace("{to}", &price_list::city_of(&to));
        let places = [Some(from), from_address, Some(to), to_address].into_iter().flatten().collect();
        Some((question, places))
    }

    /// The price of the ride in `input`, from the business's price bot. `None`: no bot set up.
    async fn price_from_bot(
        &self,
        business: &Business,
        action: &str,
        input: &Value,
        query: &str,
        cache_minutes: u64,
        timeout: Duration,
    ) -> Option<Result<Value, ActionFailure>> {
        let bot = self.chat_bot.as_ref()?;
        let Some((question, places)) = Self::price_question(query, input) else {
            return Some(Err(ActionFailure::failed(format!("{action}: the route is not known"))));
        };
        let fresh = Duration::from_secs(cache_minutes * 60);
        let answer = match bot.ask(&business.config.id, &question, "₪", timeout, fresh, false).await {
            Ok(Some(a)) => a,
            Ok(None) => return None,
            Err(e) => return Some(Err(ActionFailure::failed(format!("{action}: the price bot: {e:#}")))),
        };
        let Some(list) = price_list::parse(&answer.text) else {
            return Some(Err(ActionFailure::failed(format!("{action}: no prices in the bot's answer"))));
        };
        // Who is coming is known now, whenever the list was asked for.
        let slots = &input["slots"];
        let passengers = slots["passengers"].as_u64().and_then(|p| u32::try_from(p).ok());
        let round_trip = slots["round_trip"].as_bool().unwrap_or(false);
        let places: Vec<&str> = places.iter().map(String::as_str).collect();
        let Some(mut quote) = list.quote(passengers, round_trip, &places) else {
            return Some(Err(ActionFailure::failed(format!("{action}: no cars in the bot's answer"))));
        };
        // "כמה זמן נסיעה מביתר לירושלים?": the time and distance the list gives, not the price.
        let about = slots["asks_about"].as_str().unwrap_or("price");
        if matches!(about, "time" | "distance") && list.duration.is_some() && list.distance_km.is_some() {
            quote["response"] = json!("ride_time_answer");
        }
        quote["question"] = json!(question);
        quote["answer"] = json!(answer.text);
        quote["cached"] = json!(answer.remembered);
        quote["asked_at"] = json!(answer.asked_at);
        Some(Ok(quote))
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
                ActionBackend::PriceBot { query, cache_minutes } => {
                    match self.price_from_bot(business, action, &input, query, *cache_minutes, timeout).await {
                        Some(result) => return result,
                        None => continue,
                    }
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

    async fn warm(&self, business: &Business, action: &str, input: Value) {
        let (Some(bot), Some(config)) = (&self.chat_bot, business.config.actions.get(action)) else { return };
        for backend in &config.backends {
            if let ActionBackend::PriceBot { query, cache_minutes } = backend {
                let Some((question, _)) = Self::price_question(query, &input) else { return };
                let fresh = Duration::from_secs(cache_minutes * 60);
                let timeout = Duration::from_millis(config.timeout_ms);
                match bot.ask(&business.config.id, &question, "₪", timeout, fresh, true).await {
                    Ok(Some(a)) if !a.remembered => tracing::info!(question, "price list asked ahead of the caller"),
                    Ok(_) => {}
                    Err(e) => tracing::info!(question, error = %format!("{e:#}"), "price list not asked ahead"),
                }
                return;
            }
        }
    }
}

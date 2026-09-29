//! Orders to WhatsApp. Accounts are signed in on the WhatsApp service (`whatsapp/`, one QR
//! scan each); each account sends to its own list of groups and saved contacts. Every new
//! order becomes one message per target that wants it, queued in Postgres and sent one at a
//! time per account at a human pace (a random wait between messages, "typing…" first,
//! hourly and daily limits, quiet hours, half the limits while a new number warms up), so a
//! number is not blocked for sending like a bot. Nothing here slows a call: the queue is
//! written with the order and drained in the background.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use chrono::{DateTime, NaiveTime, Timelike, Utc};
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{PgPool, Row};

use crate::server::{authorized, AppState};

const ISRAEL: chrono_tz::Tz = chrono_tz::Asia::Jerusalem;

/// The events a target can receive.
pub const EVENTS: [&str; 3] = ["order", "order_verify", "handoff"];

// ---------------------------------------------------------------------------------------
// The WhatsApp service

pub struct Service {
    http: reqwest::Client,
    base: String,
    token: String,
}

#[derive(Debug)]
pub enum ServiceError {
    /// Refused for a reason that will not change by trying again (not a contact, not a
    /// member of the group, too many accounts).
    Refused(String),
    /// The account is not connected right now.
    NotReady,
    NotFound,
    Unavailable(String),
    /// The request may have reached WhatsApp (the service says so, or no answer came in
    /// time): a message may be out, so it is not sent again on its own.
    OutcomeUnknown(String),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Session {
    pub id: String,
    pub name: String,
    pub status: String,
    pub me: Option<Value>,
    pub error: Option<String>,
    pub first_ready_at: Option<DateTime<Utc>>,
}

impl Service {
    pub fn new(base: String, token: String) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_default();
        Self { http, base: base.trim_end_matches('/').to_string(), token }
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Value, ServiceError> {
        let mut req = self.http.request(method, format!("{}{path}", self.base)).header("x-internal-token", &self.token);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let res = req.send().await.map_err(|e| {
            if e.is_timeout() {
                ServiceError::OutcomeUnknown(e.to_string())
            } else {
                ServiceError::Unavailable(e.to_string())
            }
        })?;
        let status = res.status();
        let value: Value =
            if status == StatusCode::NO_CONTENT { Value::Null } else { res.json().await.unwrap_or(Value::Null) };
        let code = value["error"].as_str().unwrap_or("").to_string();
        match status.as_u16() {
            200..=299 => Ok(value),
            403 => Err(ServiceError::Refused(code)),
            404 => Err(ServiceError::NotFound),
            409 => Err(ServiceError::NotReady),
            502 if code == "outcome_unknown" => Err(ServiceError::OutcomeUnknown(code)),
            _ => Err(ServiceError::Unavailable(format!("{status} {code}"))),
        }
    }

    pub async fn sessions(&self) -> Result<(Vec<Session>, u64), ServiceError> {
        let v = self.call(reqwest::Method::GET, "/sessions", None).await?;
        let list =
            serde_json::from_value(v["sessions"].clone()).map_err(|e| ServiceError::Unavailable(e.to_string()))?;
        Ok((list, v["max"].as_u64().unwrap_or(5)))
    }

    pub async fn chats(&self, id: &str) -> Result<Value, ServiceError> {
        self.call(reqwest::Method::GET, &format!("/sessions/{}/chats", enc(id)), None).await
    }

    pub async fn send(&self, id: &str, chat_id: &str, text: &str, typing_ms: u64) -> Result<(), ServiceError> {
        let body = json!({ "chat_id": chat_id, "text": text, "typing_ms": typing_ms });
        self.call(reqwest::Method::POST, &format!("/sessions/{}/send", enc(id)), Some(body)).await.map(|_| ())
    }
}

fn enc(id: &str) -> String {
    id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect()
}

// ---------------------------------------------------------------------------------------
// Pace

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Pace {
    /// A random wait between two messages, in seconds.
    pub min_delay_s: u32,
    pub max_delay_s: u32,
    /// "typing…" for 1–3 seconds before each message.
    pub typing: bool,
    pub per_hour: u32,
    pub per_day: u32,
    /// Nothing sent between these times (Israel), when on.
    pub quiet: bool,
    pub quiet_from: String,
    pub quiet_to: String,
    /// Half the limits for this many days after the number was first connected.
    pub warmup_days: u32,
}

impl Default for Pace {
    fn default() -> Self {
        Self {
            min_delay_s: 8,
            max_delay_s: 15,
            typing: true,
            per_hour: 60,
            per_day: 400,
            quiet: false,
            quiet_from: "22:00".into(),
            quiet_to: "07:00".into(),
            warmup_days: 3,
        }
    }
}

impl Pace {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !(3..=600).contains(&self.min_delay_s) || self.max_delay_s < self.min_delay_s || self.max_delay_s > 900 {
            return Err("the wait between messages is 3–600 seconds, the most at least the least");
        }
        if !(1..=500).contains(&self.per_hour) || !(1..=5000).contains(&self.per_day) {
            return Err("the limits are 1–500 an hour and 1–5000 a day");
        }
        if self.warmup_days > 30 {
            return Err("warm-up is at most 30 days");
        }
        for t in [&self.quiet_from, &self.quiet_to] {
            NaiveTime::parse_from_str(t, "%H:%M").map_err(|_| "quiet hours are HH:MM")?;
        }
        Ok(())
    }

    /// Whether `now` (Israel time) is in the quiet hours, which may run past midnight.
    pub fn quiet_at(&self, now: NaiveTime) -> bool {
        let (Ok(from), Ok(to)) =
            (NaiveTime::parse_from_str(&self.quiet_from, "%H:%M"), NaiveTime::parse_from_str(&self.quiet_to, "%H:%M"))
        else {
            return false;
        };
        self.quiet && if from <= to { now >= from && now < to } else { now >= from || now < to }
    }

    /// The hourly and daily limits, halved while the number warms up.
    pub fn limits(&self, warming: bool) -> (u32, u32) {
        if warming {
            ((self.per_hour / 2).max(1), (self.per_day / 2).max(1))
        } else {
            (self.per_hour, self.per_day)
        }
    }

    pub fn warming(&self, first_ready_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
        first_ready_at.is_none_or(|t| now - t < chrono::Duration::days(i64::from(self.warmup_days)))
    }
}

// ---------------------------------------------------------------------------------------
// Messages

fn phone(p: &str) -> String {
    match p.strip_prefix("+972") {
        Some(rest) if rest.len() == 9 && rest.chars().all(|c| c.is_ascii_digit()) => {
            format!("0{}-{}-{}", &rest[..2], &rest[2..5], &rest[5..])
        }
        _ => p.to_string(),
    }
}

fn now_label(now: DateTime<Utc>) -> String {
    now.with_timezone(&ISRAEL).format("%d.%m %H:%M").to_string()
}

/// An order card as a WhatsApp message.
pub fn order_message(card: &Value, now: DateTime<Utc>) -> String {
    let task = card["task"].as_str().unwrap_or("הזמנה");
    let mut lines = vec![format!("🚕 {task} · {}", now_label(now))];
    for d in card["details"].as_array().into_iter().flatten() {
        let (Some(label), Some(value)) = (d["label"].as_str(), d["value"].as_str()) else { continue };
        let unverified = d["address"].as_str().is_some_and(|a| a.contains("לא מאומת"));
        lines.push(format!("{label}: {value}{}", if unverified { " (מקום לא מאומת)" } else { "" }));
    }
    if let Some(p) = card["phone"].as_str().filter(|p| !p.is_empty()) {
        lines.push(format!("📞 {}", phone(p)));
    }
    if let Some(id) = card["result"]["ride_id"].as_str() {
        lines.push(format!("מספר הזמנה: {id}"));
    }
    if card["verify"].as_bool() == Some(true) {
        lines.push("⚠️ לבדיקה: לא ידוע אם ההזמנה נקלטה במערכת ההזמנות".into());
    }
    lines.join("\n")
}

pub fn handoff_message(reason: &str, text: &str, now: DateTime<Utc>) -> String {
    let mut lines = vec![format!("📞 שיחה הועברה למוקדן · {}", now_label(now))];
    if !text.trim().is_empty() {
        lines.push(text.trim().to_string());
    }
    lines.push(format!("סיבה: {reason}"));
    lines.join("\n")
}

/// Queues `text` for every target that receives `event`. Called with the order it belongs to.
pub async fn enqueue(pool: &PgPool, event: &str, text: &str) -> sqlx::Result<u64> {
    let r = sqlx::query(
        "INSERT INTO callora_v2.whatsapp_outbox (account_id, chat_id, chat_name, event, text)
         SELECT account_id, chat_id, chat_name, $1, $2 FROM callora_v2.whatsapp_targets WHERE $1 = ANY(events)",
    )
    .bind(event)
    .bind(text)
    .execute(pool)
    .await?;
    Ok(r.rows_affected())
}

// ---------------------------------------------------------------------------------------
// The sender

async fn pace_of(pool: &PgPool, account: &str) -> Pace {
    sqlx::query("SELECT settings FROM callora_v2.whatsapp_accounts WHERE id = $1")
        .bind(account)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .and_then(|r| serde_json::from_value(r.get::<Value, _>("settings")).ok())
        .unwrap_or_default()
}

/// Messages an account sent in the last hour and since midnight in Israel.
async fn sent_counts(pool: &PgPool, account: &str, now: DateTime<Utc>) -> sqlx::Result<(i64, i64)> {
    let midnight = now
        .with_timezone(&ISRAEL)
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|m| m.and_local_timezone(ISRAEL).earliest())
        .map_or(now - chrono::Duration::hours(24), |m| m.with_timezone(&Utc));
    let row = sqlx::query(
        "SELECT count(*) FILTER (WHERE sent_at > $2 - interval '1 hour') AS hour,
                count(*) FILTER (WHERE sent_at >= $3) AS day
         FROM callora_v2.whatsapp_outbox WHERE account_id = $1 AND status = 'sent' AND sent_at >= $3 - interval '1 hour'",
    )
    .bind(account)
    .bind(now)
    .bind(midnight)
    .fetch_one(pool)
    .await?;
    Ok((row.get("hour"), row.get("day")))
}

/// Retries after a failure that may pass: 30 s, 1, 2, 4, 8, then every 10 minutes.
fn backoff(attempts: i32) -> chrono::Duration {
    chrono::Duration::seconds((30i64 << attempts.clamp(0, 5)).min(600))
}

const MAX_ATTEMPTS: i32 = 40;

/// Drains the queue for as long as the server runs.
pub async fn run_sender(pool: PgPool, service: Arc<Service>) {
    let mut next_allowed: HashMap<String, Instant> = HashMap::new();
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let Ok((sessions, _)) = service.sessions().await else { continue };
        for s in sessions.iter().filter(|s| s.status == "ready") {
            if next_allowed.get(&s.id).is_some_and(|t| Instant::now() < *t) {
                continue;
            }
            let now = Utc::now();
            let pace = pace_of(&pool, &s.id).await;
            if pace.quiet_at(now.with_timezone(&ISRAEL).time().with_nanosecond(0).unwrap_or_default()) {
                continue;
            }
            let Ok((hour, day)) = sent_counts(&pool, &s.id, now).await else { continue };
            let (per_hour, per_day) = pace.limits(pace.warming(s.first_ready_at, now));
            if hour >= i64::from(per_hour) || day >= i64::from(per_day) {
                continue;
            }
            let next = sqlx::query(
                "SELECT id, chat_id, text, attempts FROM callora_v2.whatsapp_outbox
                 WHERE account_id = $1 AND status = 'pending' AND next_attempt_at <= now() ORDER BY id LIMIT 1",
            )
            .bind(&s.id)
            .fetch_optional(&pool)
            .await;
            let Ok(Some(row)) = next else { continue };
            let (id, chat_id, text, attempts): (i64, String, String, i32) =
                (row.get("id"), row.get("chat_id"), row.get("text"), row.get("attempts"));
            let typing = if pace.typing { rand::thread_rng().gen_range(1000..=3000) } else { 0 };
            let result = service.send(&s.id, &chat_id, &text, typing).await;
            let update = match &result {
                Ok(()) => sqlx::query(
                    "UPDATE callora_v2.whatsapp_outbox SET status = 'sent', sent_at = now(), attempts = attempts + 1, last_error = NULL WHERE id = $1",
                )
                .bind(id),
                Err(ServiceError::Refused(code)) => sqlx::query(
                    "UPDATE callora_v2.whatsapp_outbox SET status = 'failed', attempts = attempts + 1, last_error = $2 WHERE id = $1",
                )
                .bind(id)
                .bind(refusal(code)),
                // Maybe sent: never again on its own (each retry of a message whose answer was
                // lost reached the group). The dashboard's retry is there if it did not arrive.
                Err(ServiceError::OutcomeUnknown(_)) => sqlx::query(
                    "UPDATE callora_v2.whatsapp_outbox SET status = 'failed', attempts = attempts + 1, last_error = $2 WHERE id = $1",
                )
                .bind(id)
                .bind("ייתכן שנשלח: לא נשלח שוב כדי שלא תהיה כפילות. אם לא הגיע, נסו שוב"),
                Err(e) => {
                    let failed = attempts + 1 >= MAX_ATTEMPTS;
                    sqlx::query(
                        "UPDATE callora_v2.whatsapp_outbox SET attempts = attempts + 1, last_error = $2,
                           status = CASE WHEN $3 THEN 'failed' ELSE status END, next_attempt_at = now() + $4 WHERE id = $1",
                    )
                    .bind(id)
                    .bind(match e {
                        ServiceError::NotReady => "החשבון לא מחובר".to_string(),
                        ServiceError::NotFound => "החשבון לא נמצא".to_string(),
                        ServiceError::Unavailable(m) | ServiceError::Refused(m) | ServiceError::OutcomeUnknown(m) => {
                            format!("שירות הוואטסאפ לא זמין ({m})")
                        }
                    })
                    .bind(failed)
                    .bind(backoff(attempts))
                }
            };
            if let Err(e) = update.execute(&pool).await {
                tracing::error!(error = %e, "whatsapp outbox update failed");
            }
            match result {
                Ok(()) => tracing::info!(account = %s.id, message = id, "whatsapp message sent"),
                Err(e) => tracing::warn!(account = %s.id, message = id, error = ?e, "whatsapp message not sent"),
            }
            // The wait before this account's next message, random so it does not look scheduled.
            let wait = rand::thread_rng().gen_range(pace.min_delay_s..=pace.max_delay_s.max(pace.min_delay_s));
            next_allowed.insert(s.id.clone(), Instant::now() + Duration::from_secs(u64::from(wait)));
        }
    }
}

fn refusal(code: &str) -> String {
    match code {
        "not_a_contact" => "היעד כבר לא איש קשר שמור".into(),
        "not_a_member" => "החשבון כבר לא חבר בקבוצה".into(),
        other => format!("נדחה ({other})"),
    }
}

// ---------------------------------------------------------------------------------------
// The dashboard's API: /api/whatsapp/…

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/whatsapp", get(overview))
        .route("/api/whatsapp/accounts", post(add_account))
        .route("/api/whatsapp/accounts/{id}", delete(remove_account))
        .route("/api/whatsapp/accounts/{id}/restart", post(restart_account))
        .route("/api/whatsapp/accounts/{id}/qr", get(qr))
        .route("/api/whatsapp/accounts/{id}/chats", get(chats))
        .route("/api/whatsapp/accounts/{id}/settings", put(save_settings))
        .route("/api/whatsapp/accounts/{id}/targets", put(save_targets))
        .route("/api/whatsapp/accounts/{id}/test", post(test_message))
        .route("/api/whatsapp/outbox", get(outbox))
        .route("/api/whatsapp/outbox/{id}/retry", post(retry))
}

type Checked<'a> = Result<(&'a Service, &'a PgPool), Box<Response>>;

fn check<'a>(s: &'a AppState, headers: &HeaderMap) -> Checked<'a> {
    if !authorized(s, headers) {
        return Err(Box::new(StatusCode::UNAUTHORIZED.into_response()));
    }
    let Some(service) = s.whatsapp.as_deref() else {
        return Err(Box::new(
            (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "not_configured" }))).into_response(),
        ));
    };
    let Some(pool) = &s.db else {
        return Err(Box::new(
            (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "no_database" }))).into_response(),
        ));
    };
    Ok((service, pool))
}

fn service_error(e: ServiceError) -> Response {
    match e {
        ServiceError::Refused(code) => (StatusCode::FORBIDDEN, Json(json!({ "error": code }))).into_response(),
        ServiceError::NotReady => (StatusCode::CONFLICT, Json(json!({ "error": "not_ready" }))).into_response(),
        ServiceError::NotFound => StatusCode::NOT_FOUND.into_response(),
        ServiceError::Unavailable(m) | ServiceError::OutcomeUnknown(m) => {
            tracing::warn!(error = %m, "whatsapp service unavailable");
            (StatusCode::BAD_GATEWAY, Json(json!({ "error": "unavailable" }))).into_response()
        }
    }
}

fn db_error(e: sqlx::Error) -> Response {
    tracing::error!(error = %e, "whatsapp query failed");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

async fn overview(State(s): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !authorized(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if s.whatsapp.is_none() {
        return Json(json!({ "configured": false })).into_response();
    }
    let (service, pool) = match check(&s, &headers) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let (sessions, max) = match service.sessions().await {
        Ok(v) => v,
        Err(_) => return Json(json!({ "configured": true, "reachable": false, "accounts": [] })).into_response(),
    };
    let now = Utc::now();
    let mut accounts = Vec::new();
    for sess in sessions {
        let pace = pace_of(pool, &sess.id).await;
        let warming = pace.warming(sess.first_ready_at, now);
        let (per_hour, per_day) = pace.limits(warming);
        let (hour, day) = sent_counts(pool, &sess.id, now).await.unwrap_or((0, 0));
        let queue = sqlx::query(
            "SELECT count(*) AS pending, extract(epoch FROM now() - min(created_at))::float8 / 60 AS oldest
             FROM callora_v2.whatsapp_outbox WHERE account_id = $1 AND status = 'pending'",
        )
        .bind(&sess.id)
        .fetch_one(pool)
        .await;
        let (pending, oldest): (i64, Option<f64>) =
            queue.map(|r| (r.get("pending"), r.get("oldest"))).unwrap_or((0, None));
        let targets = sqlx::query(
            "SELECT chat_id, chat_name, kind, events FROM callora_v2.whatsapp_targets WHERE account_id = $1 ORDER BY id",
        )
        .bind(&sess.id)
        .fetch_all(pool)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            json!({
                "chat_id": r.get::<String, _>("chat_id"),
                "chat_name": r.get::<String, _>("chat_name"),
                "kind": r.get::<String, _>("kind"),
                "events": r.get::<Vec<String>, _>("events"),
            })
        })
        .collect::<Vec<_>>();
        let quiet_now = pace.quiet_at(now.with_timezone(&ISRAEL).time());
        accounts.push(json!({
            "id": sess.id,
            "name": sess.name,
            "status": sess.status,
            "me": sess.me,
            "error": sess.error,
            "first_ready_at": sess.first_ready_at,
            "settings": pace,
            "warming_up": warming,
            "targets": targets,
            "queue": {
                "pending": pending,
                "oldest_pending_minutes": oldest,
                "sent_hour": hour,
                "sent_today": day,
                "limit_hour": per_hour,
                "limit_day": per_day,
                "quiet_now": quiet_now,
            },
        }));
    }
    Json(json!({ "configured": true, "reachable": true, "max": max, "accounts": accounts })).into_response()
}

#[derive(Deserialize)]
struct NewAccount {
    name: String,
}

async fn add_account(State(s): State<Arc<AppState>>, headers: HeaderMap, Json(b): Json<NewAccount>) -> Response {
    let (service, _) = match check(&s, &headers) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let name: String = b.name.trim().chars().take(60).collect();
    match service.call(reqwest::Method::POST, "/sessions", Some(json!({ "name": name }))).await {
        Ok(v) => (StatusCode::CREATED, Json(v)).into_response(),
        Err(e) => service_error(e),
    }
}

async fn remove_account(State(s): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    let (service, pool) = match check(&s, &headers) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    if let Err(e) = service.call(reqwest::Method::DELETE, &format!("/sessions/{}", enc(&id)), None).await {
        if !matches!(e, ServiceError::NotFound) {
            return service_error(e);
        }
    }
    let cleanup = async {
        sqlx::query("DELETE FROM callora_v2.whatsapp_targets WHERE account_id = $1").bind(&id).execute(pool).await?;
        sqlx::query("DELETE FROM callora_v2.whatsapp_accounts WHERE id = $1").bind(&id).execute(pool).await?;
        sqlx::query(
            "UPDATE callora_v2.whatsapp_outbox SET status = 'failed', last_error = 'החשבון נמחק' WHERE account_id = $1 AND status = 'pending'",
        )
        .bind(&id)
        .execute(pool)
        .await
    };
    match cleanup.await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => db_error(e),
    }
}

async fn restart_account(State(s): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    let (service, _) = match check(&s, &headers) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    match service.call(reqwest::Method::POST, &format!("/sessions/{}/restart", enc(&id)), None).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => service_error(e),
    }
}

async fn qr(State(s): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    let (service, _) = match check(&s, &headers) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    match service.call(reqwest::Method::GET, &format!("/sessions/{}/qr", enc(&id)), None).await {
        Ok(v) => ([(axum::http::header::CACHE_CONTROL, "no-store")], Json(v)).into_response(),
        Err(e) => service_error(e),
    }
}

async fn chats(State(s): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    let (service, _) = match check(&s, &headers) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    match service.chats(&id).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => service_error(e),
    }
}

async fn save_settings(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(pace): Json<Pace>,
) -> Response {
    let (_, pool) = match check(&s, &headers) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    if let Err(message) = pace.validate() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response();
    }
    let r = sqlx::query(
        "INSERT INTO callora_v2.whatsapp_accounts (id, settings) VALUES ($1, $2)
         ON CONFLICT (id) DO UPDATE SET settings = EXCLUDED.settings",
    )
    .bind(&id)
    .bind(json!(pace))
    .execute(pool)
    .await;
    match r {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => db_error(e),
    }
}

#[derive(Deserialize)]
struct TargetIn {
    chat_id: String,
    events: Vec<String>,
}

/// Replaces an account's targets. Each must be one of the account's groups or saved
/// contacts right now; its name and kind come from WhatsApp, not from the page.
async fn save_targets(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(targets): Json<Vec<TargetIn>>,
) -> Response {
    let (service, pool) = match check(&s, &headers) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let chats = match service.chats(&id).await {
        Ok(v) => v,
        Err(e) => return service_error(e),
    };
    let mut known: HashMap<String, (String, &str)> = HashMap::new();
    for g in chats["groups"].as_array().into_iter().flatten() {
        if let (Some(i), Some(n)) = (g["id"].as_str(), g["name"].as_str()) {
            known.insert(i.to_string(), (n.to_string(), "group"));
        }
    }
    for c in chats["contacts"].as_array().into_iter().flatten() {
        if let (Some(i), Some(n)) = (c["id"].as_str(), c["name"].as_str()) {
            known.insert(i.to_string(), (n.to_string(), "contact"));
        }
    }
    let mut rows = Vec::new();
    for t in targets.iter().take(50) {
        let Some((name, kind)) = known.get(&t.chat_id) else {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": "not_allowed", "chat_id": t.chat_id })))
                .into_response();
        };
        let events: Vec<String> = t.events.iter().filter(|e| EVENTS.contains(&e.as_str())).cloned().collect();
        rows.push((t.chat_id.clone(), name.clone(), *kind, events));
    }
    let save = async {
        let mut tx = pool.begin().await?;
        sqlx::query("DELETE FROM callora_v2.whatsapp_targets WHERE account_id = $1")
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        for (chat_id, name, kind, events) in &rows {
            sqlx::query(
                "INSERT INTO callora_v2.whatsapp_targets (account_id, chat_id, chat_name, kind, events) VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(&id)
            .bind(chat_id)
            .bind(name)
            .bind(kind)
            .bind(events)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await
    };
    match save.await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => db_error(e),
    }
}

#[derive(Deserialize)]
struct TestIn {
    chat_id: String,
}

/// A test message to one of the account's targets, through the same queue and pace.
async fn test_message(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(b): Json<TestIn>,
) -> Response {
    let (_, pool) = match check(&s, &headers) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let text = format!("✅ הודעת בדיקה מקלורה · {}\nמכאן יגיעו ההזמנות.", now_label(Utc::now()));
    let r = sqlx::query(
        "INSERT INTO callora_v2.whatsapp_outbox (account_id, chat_id, chat_name, event, text)
         SELECT account_id, chat_id, chat_name, 'test', $3 FROM callora_v2.whatsapp_targets WHERE account_id = $1 AND chat_id = $2",
    )
    .bind(&id)
    .bind(&b.chat_id)
    .bind(text)
    .execute(pool)
    .await;
    match r {
        Ok(done) if done.rows_affected() == 1 => StatusCode::ACCEPTED.into_response(),
        Ok(_) => (StatusCode::BAD_REQUEST, Json(json!({ "error": "not_a_target" }))).into_response(),
        Err(e) => db_error(e),
    }
}

#[derive(Deserialize)]
struct OutboxQuery {
    limit: Option<i64>,
}

async fn outbox(State(s): State<Arc<AppState>>, headers: HeaderMap, Query(q): Query<OutboxQuery>) -> Response {
    let (_, pool) = match check(&s, &headers) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let rows = sqlx::query(
        "SELECT id, account_id, chat_name, event, text, status, attempts, last_error, created_at, sent_at, next_attempt_at
         FROM callora_v2.whatsapp_outbox ORDER BY id DESC LIMIT $1",
    )
    .bind(q.limit.unwrap_or(100).clamp(1, 500))
    .fetch_all(pool)
    .await;
    match rows {
        Ok(rows) => Json(
            rows.iter()
                .map(|r| {
                    json!({
                        "id": r.get::<i64, _>("id"),
                        "account_id": r.get::<String, _>("account_id"),
                        "chat_name": r.get::<String, _>("chat_name"),
                        "event": r.get::<String, _>("event"),
                        "text": r.get::<String, _>("text"),
                        "status": r.get::<String, _>("status"),
                        "attempts": r.get::<i32, _>("attempts"),
                        "last_error": r.get::<Option<String>, _>("last_error"),
                        "created_at": r.get::<DateTime<Utc>, _>("created_at"),
                        "sent_at": r.get::<Option<DateTime<Utc>>, _>("sent_at"),
                        "next_attempt_at": r.get::<DateTime<Utc>, _>("next_attempt_at"),
                    })
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => db_error(e),
    }
}

async fn retry(State(s): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    let (_, pool) = match check(&s, &headers) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let r = sqlx::query(
        "UPDATE callora_v2.whatsapp_outbox SET status = 'pending', attempts = 0, next_attempt_at = now(), last_error = NULL
         WHERE id = $1 AND status IN ('failed', 'pending')",
    )
    .bind(id)
    .execute(pool)
    .await;
    match r {
        Ok(done) if done.rows_affected() == 1 => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => db_error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_order_card_reads_as_a_message() {
        let card = json!({
            "task": "הזמנת מונית", "phone": "+972545460223", "verify": true, "result": { "ride_id": "DEMO-1001" },
            "details": [
                { "field": "pickup", "label": "כתובת איסוף", "value": "בן זכאי 45, אלעד", "address": "רבן יוחנן בן זכאי 45, אלעד" },
                { "field": "destination", "label": "יעד", "value": "סוכות 72, ירושלים", "address": "סוכות 72, ירושלים (מקום לא מאומת: לתאם עם הנוסע)" },
                { "field": "passengers", "label": "מספר נוסעים", "value": "2", "address": null }
            ]
        });
        let now = DateTime::parse_from_rfc3339("2026-09-29T11:36:00Z").unwrap().with_timezone(&Utc);
        let m = order_message(&card, now);
        assert_eq!(
            m,
            "🚕 הזמנת מונית · 29.09 14:36\nכתובת איסוף: בן זכאי 45, אלעד\nיעד: סוכות 72, ירושלים (מקום לא מאומת)\n\
             מספר נוסעים: 2\n📞 054-546-0223\nמספר הזמנה: DEMO-1001\n⚠️ לבדיקה: לא ידוע אם ההזמנה נקלטה במערכת ההזמנות"
        );
    }

    #[test]
    fn the_pace_keeps_quiet_hours_across_midnight_and_halves_limits_while_warming_up() {
        let pace = Pace { quiet: true, ..Pace::default() };
        let t = |h, m| NaiveTime::from_hms_opt(h, m, 0).unwrap();
        assert!(
            pace.quiet_at(t(23, 0)) && pace.quiet_at(t(3, 0)) && !pace.quiet_at(t(7, 0)) && !pace.quiet_at(t(12, 0))
        );
        assert!(!Pace::default().quiet_at(t(23, 0)), "off by default");
        assert_eq!(pace.limits(true), (30, 200));
        assert_eq!(pace.limits(false), (60, 400));
        let now = Utc::now();
        assert!(pace.warming(None, now));
        assert!(pace.warming(Some(now - chrono::Duration::days(2)), now));
        assert!(!pace.warming(Some(now - chrono::Duration::days(4)), now));
    }

    #[test]
    fn a_pace_out_of_range_is_refused() {
        assert!(Pace::default().validate().is_ok());
        assert!(Pace { min_delay_s: 1, ..Pace::default() }.validate().is_err());
        assert!(Pace { min_delay_s: 20, max_delay_s: 10, ..Pace::default() }.validate().is_err());
        assert!(Pace { quiet_from: "25:00".into(), ..Pace::default() }.validate().is_err());
        assert!(Pace { per_hour: 0, ..Pace::default() }.validate().is_err());
    }

    #[test]
    fn retries_slow_down_to_every_ten_minutes() {
        assert_eq!(backoff(0).num_seconds(), 30);
        assert_eq!(backoff(1).num_seconds(), 60);
        assert_eq!(backoff(4).num_seconds(), 480);
        assert_eq!(backoff(9).num_seconds(), 600);
    }
}

//! PostgreSQL persistence for calls, turns, actions and handoffs.
//!
//! Calls never wait on the database: records go into a bounded queue drained by one
//! writer task. If the database is down or slow the queue fills and records are dropped
//! (and counted in the log), while the call carries on.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Row;
use tokio::sync::mpsc;

use crate::ports::{CallRecord, CallStore};

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

pub async fn connect(url: &str) -> anyhow::Result<PgPool> {
    let pool = PgPoolOptions::new().max_connections(8).acquire_timeout(Duration::from_secs(5)).connect(url).await?;
    Ok(pool)
}

pub async fn migrate(pool: &PgPool) -> anyhow::Result<()> {
    MIGRATOR.run(pool).await?;
    Ok(())
}

pub struct PgStore {
    tx: mpsc::Sender<CallRecord>,
    dropped: Arc<AtomicU64>,
}

impl PgStore {
    pub fn spawn(pool: PgPool) -> Self {
        let (tx, mut rx) = mpsc::channel::<CallRecord>(4096);
        tokio::spawn(async move {
            while let Some(record) = rx.recv().await {
                if let Err(e) = write(&pool, &record).await {
                    tracing::warn!(error = %e, "call record not stored");
                }
            }
        });
        Self { tx, dropped: Arc::new(AtomicU64::new(0)) }
    }
}

impl CallStore for PgStore {
    fn record(&self, record: CallRecord) {
        if self.tx.try_send(record).is_err() {
            let n = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_power_of_two() {
                tracing::warn!(dropped = n, "database writer is behind; call records dropped");
            }
        }
    }
}

async fn write(pool: &PgPool, record: &CallRecord) -> sqlx::Result<()> {
    match record {
        CallRecord::Started { info } => {
            sqlx::query(
                "INSERT INTO callora_v2.calls (id, call_sid, business_id, from_number, to_number)
                 VALUES ($1, $2, $3, $4, $5) ON CONFLICT (call_sid) DO NOTHING",
            )
            .bind(info.call_id)
            .bind(&info.call_sid)
            .bind(&info.business_id)
            .bind(&info.from)
            .bind(&info.to)
            .execute(pool)
            .await?;
        }
        CallRecord::Turn { call_id, speaker, text, detail } => {
            sqlx::query("INSERT INTO callora_v2.call_turns (call_id, speaker, text, detail) VALUES ($1, $2, $3, $4)")
                .bind(call_id)
                .bind(speaker)
                .bind(text)
                .bind(detail)
                .execute(pool)
                .await?;
        }
        CallRecord::Action { call_id, action, input, result, ok, latency_ms } => {
            sqlx::query(
                "INSERT INTO callora_v2.action_runs (call_id, action, input, result, ok, latency_ms) VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(call_id)
            .bind(action)
            .bind(input)
            .bind(result)
            .bind(ok)
            .bind(i32::try_from(*latency_ms).unwrap_or(i32::MAX))
            .execute(pool)
            .await?;
        }
        CallRecord::Handoff { call_id, summary } => {
            sqlx::query("INSERT INTO callora_v2.handoffs (call_id, reason, summary) VALUES ($1, $2, $3)")
                .bind(call_id)
                .bind(&summary.reason)
                .bind(serde_json::to_value(summary).unwrap_or(Value::Null))
                .execute(pool)
                .await?;
        }
        CallRecord::Ended { call_id, outcome, state, usage } => {
            sqlx::query(
                "UPDATE callora_v2.calls SET ended_at = now(), outcome = $2, final_state = $3, llm_usage = $4 WHERE id = $1",
            )
            .bind(call_id)
            .bind(outcome)
            .bind(state)
            .bind((usage.input + usage.output > 0).then(|| serde_json::to_value(usage).unwrap_or(Value::Null)))
            .execute(pool)
            .await?;
        }
        CallRecord::Utterance { call_id, heard, audio } => {
            sqlx::query("INSERT INTO callora_v2.utterances (call_id, heard, audio) VALUES ($1, $2, $3)")
                .bind(call_id)
                .bind(heard)
                .bind(audio)
                .execute(pool)
                .await?;
        }
        CallRecord::Order { call_id, card } => {
            sqlx::query("INSERT INTO callora_v2.orders (call_id, card) VALUES ($1, $2)")
                .bind(call_id)
                .bind(card)
                .execute(pool)
                .await?;
        }
        CallRecord::Status { call_sid, status, duration_seconds } => {
            sqlx::query("UPDATE callora_v2.calls SET twilio_status = $2, duration_seconds = COALESCE($3, duration_seconds) WHERE call_sid = $1")
                .bind(call_sid)
                .bind(status)
                .bind(duration_seconds)
                .execute(pool)
                .await?;
        }
    }
    Ok(())
}

/// Delete transcripts and recorded utterances older than `days` (0 keeps them forever).
/// Callers' own words and voices age out.
pub async fn prune_transcripts(pool: &PgPool, days: u32) -> sqlx::Result<u64> {
    if days == 0 {
        return Ok(0);
    }
    let days = i32::try_from(days).unwrap_or(i32::MAX);
    let turns = sqlx::query("DELETE FROM callora_v2.call_turns WHERE at < now() - make_interval(days => $1)")
        .bind(days)
        .execute(pool)
        .await?;
    let audio = sqlx::query("DELETE FROM callora_v2.utterances WHERE at < now() - make_interval(days => $1)")
        .bind(days)
        .execute(pool)
        .await?;
    Ok(turns.rows_affected() + audio.rows_affected())
}

// ---------------------------------------------------------------------------------------
// Reads for the admin API

pub async fn list_calls(pool: &PgPool, business: Option<&str>, limit: i64, offset: i64) -> sqlx::Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT c.id, c.call_sid, c.business_id, c.from_number, c.to_number, c.started_at, c.ended_at, c.outcome,
                c.twilio_status, c.duration_seconds, c.llm_usage, r.verdict,
                (SELECT count(*) FROM callora_v2.orders o WHERE o.call_id = c.id) AS orders
         FROM callora_v2.calls c LEFT JOIN callora_v2.call_reviews r ON r.call_id = c.id
         WHERE ($1::text IS NULL OR c.business_id = $1)
         ORDER BY c.started_at DESC LIMIT $2 OFFSET $3",
    )
    .bind(business)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "id": r.get::<uuid::Uuid, _>("id"),
                "call_sid": r.get::<String, _>("call_sid"),
                "business_id": r.get::<String, _>("business_id"),
                "from": r.get::<Option<String>, _>("from_number"),
                "to": r.get::<String, _>("to_number"),
                "started_at": r.get::<chrono::DateTime<chrono::Utc>, _>("started_at"),
                "ended_at": r.get::<Option<chrono::DateTime<chrono::Utc>>, _>("ended_at"),
                "outcome": r.get::<Option<String>, _>("outcome"),
                "twilio_status": r.get::<Option<String>, _>("twilio_status"),
                "duration_seconds": r.get::<Option<i32>, _>("duration_seconds"),
                "usage": r.get::<Option<Value>, _>("llm_usage"),
                "verdict": r.get::<Option<String>, _>("verdict"),
                "orders": r.get::<i64, _>("orders"),
            })
        })
        .collect())
}

/// One call, reduced to what the numbers on the calls page need.
#[derive(Debug, Clone, Default)]
pub struct CallFacts {
    pub outcome: Option<String>,
    pub duration_seconds: Option<i32>,
    /// Tasks it completed (order cards), and how many of them are still to be checked.
    pub orders: i64,
    pub unverified: i64,
    pub usage: Option<crate::ports::Usage>,
    pub verdict: Option<String>,
    /// From the call's start to its first completed task, and the caller turns until then.
    pub booking_seconds: Option<f64>,
    pub booking_turns: Option<i64>,
}

/// The calls of the last `days` days, for the numbers on the calls page.
pub async fn call_facts(pool: &PgPool, business: Option<&str>, days: i32) -> sqlx::Result<Vec<CallFacts>> {
    let rows = sqlx::query(
        "SELECT c.outcome, c.duration_seconds, c.llm_usage, r.verdict,
                (SELECT count(*) FROM callora_v2.orders o WHERE o.call_id = c.id) AS orders,
                (SELECT count(*) FROM callora_v2.orders o WHERE o.call_id = c.id
                   AND coalesce((o.card->>'verify')::boolean, false)) AS unverified,
                extract(epoch FROM f.at - c.started_at)::float8 AS booking_seconds,
                (SELECT count(*) FROM callora_v2.call_turns t
                   WHERE t.call_id = c.id AND t.speaker = 'caller' AND t.at <= f.at) AS booking_turns
         FROM callora_v2.calls c LEFT JOIN callora_v2.call_reviews r ON r.call_id = c.id
         LEFT JOIN LATERAL (SELECT min(o.at) AS at FROM callora_v2.orders o WHERE o.call_id = c.id) f ON true
         WHERE c.started_at > now() - make_interval(days => $1) AND ($2::text IS NULL OR c.business_id = $2)
         LIMIT 50000",
    )
    .bind(days)
    .bind(business)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| CallFacts {
            outcome: r.get("outcome"),
            duration_seconds: r.get("duration_seconds"),
            orders: r.get("orders"),
            unverified: r.get("unverified"),
            usage: r.get::<Option<Value>, _>("llm_usage").and_then(|v| serde_json::from_value(v).ok()),
            verdict: r.get("verdict"),
            booking_seconds: r.get("booking_seconds"),
            // Transcripts past their retention leave no turns to count.
            booking_turns: Some(r.get::<i64, _>("booking_turns")).filter(|n| *n > 0),
        })
        .collect())
}

/// The owner's verdict on a call ("good" or "bad", with a note); replaces an earlier one.
pub async fn set_review(pool: &PgPool, call_id: uuid::Uuid, verdict: &str, note: &str) -> sqlx::Result<bool> {
    let r = sqlx::query(
        "INSERT INTO callora_v2.call_reviews (call_id, verdict, note) SELECT id, $2, $3 FROM callora_v2.calls WHERE id = $1
         ON CONFLICT (call_id) DO UPDATE SET verdict = EXCLUDED.verdict, note = EXCLUDED.note, at = now()",
    )
    .bind(call_id)
    .bind(verdict)
    .bind(note)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() > 0)
}

/// A recorded utterance's audio (μ-law 8 kHz).
pub async fn utterance_audio(pool: &PgPool, id: i64) -> sqlx::Result<Option<Vec<u8>>> {
    let row =
        sqlx::query("SELECT audio FROM callora_v2.utterances WHERE id = $1").bind(id).fetch_optional(pool).await?;
    Ok(row.map(|r| r.get("audio")))
}

/// The newest order cards, with the calling number and time.
pub async fn list_orders(pool: &PgPool, limit: i64) -> sqlx::Result<Vec<Value>> {
    let rows = sqlx::query(
        "SELECT o.card, o.at, c.from_number, c.id AS call_id FROM callora_v2.orders o
         JOIN callora_v2.calls c ON c.id = o.call_id ORDER BY o.at DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "card": r.get::<Value, _>("card"),
                "at": r.get::<chrono::DateTime<chrono::Utc>, _>("at"),
                "from": r.get::<Option<String>, _>("from_number"),
                "call_id": r.get::<uuid::Uuid, _>("call_id"),
            })
        })
        .collect())
}

pub async fn get_call(pool: &PgPool, id: uuid::Uuid) -> sqlx::Result<Option<Value>> {
    let Some(call) = sqlx::query(
        "SELECT c.id, c.call_sid, c.business_id, c.from_number, c.to_number, c.started_at, c.ended_at, c.outcome,
                c.final_state, c.llm_usage, r.verdict, r.note
         FROM callora_v2.calls c LEFT JOIN callora_v2.call_reviews r ON r.call_id = c.id WHERE c.id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };
    let turns =
        sqlx::query("SELECT speaker, text, detail, at FROM callora_v2.call_turns WHERE call_id = $1 ORDER BY id")
            .bind(id)
            .fetch_all(pool)
            .await?;
    let actions = sqlx::query(
        "SELECT action, input, result, ok, latency_ms, at FROM callora_v2.action_runs WHERE call_id = $1 ORDER BY id",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    let handoffs = sqlx::query("SELECT reason, summary, at FROM callora_v2.handoffs WHERE call_id = $1 ORDER BY id")
        .bind(id)
        .fetch_all(pool)
        .await?;
    let utterances = sqlx::query("SELECT id, heard, at FROM callora_v2.utterances WHERE call_id = $1 ORDER BY id")
        .bind(id)
        .fetch_all(pool)
        .await?;
    Ok(Some(serde_json::json!({
        "id": call.get::<uuid::Uuid, _>("id"),
        "call_sid": call.get::<String, _>("call_sid"),
        "business_id": call.get::<String, _>("business_id"),
        "from": call.get::<Option<String>, _>("from_number"),
        "to": call.get::<String, _>("to_number"),
        "started_at": call.get::<chrono::DateTime<chrono::Utc>, _>("started_at"),
        "ended_at": call.get::<Option<chrono::DateTime<chrono::Utc>>, _>("ended_at"),
        "outcome": call.get::<Option<String>, _>("outcome"),
        "final_state": call.get::<Option<Value>, _>("final_state"),
        "usage": call.get::<Option<Value>, _>("llm_usage"),
        "review": call.get::<Option<String>, _>("verdict").map(|v| serde_json::json!({
            "verdict": v,
            "note": call.get::<Option<String>, _>("note").unwrap_or_default(),
        })),
        "turns": turns.iter().map(|t| serde_json::json!({
            "speaker": t.get::<String, _>("speaker"),
            "text": t.get::<String, _>("text"),
            "detail": t.get::<Option<Value>, _>("detail"),
            "at": t.get::<chrono::DateTime<chrono::Utc>, _>("at"),
        })).collect::<Vec<_>>(),
        "actions": actions.iter().map(|a| serde_json::json!({
            "action": a.get::<String, _>("action"),
            "input": a.get::<Value, _>("input"),
            "result": a.get::<Value, _>("result"),
            "ok": a.get::<bool, _>("ok"),
            "latency_ms": a.get::<i32, _>("latency_ms"),
            "at": a.get::<chrono::DateTime<chrono::Utc>, _>("at"),
        })).collect::<Vec<_>>(),
        "handoffs": handoffs.iter().map(|h| serde_json::json!({
            "reason": h.get::<String, _>("reason"),
            "summary": h.get::<Value, _>("summary"),
            "at": h.get::<chrono::DateTime<chrono::Utc>, _>("at"),
        })).collect::<Vec<_>>(),
        "utterances": utterances.iter().map(|u| serde_json::json!({
            "id": u.get::<i64, _>("id"),
            "heard": u.get::<String, _>("heard"),
            "at": u.get::<chrono::DateTime<chrono::Utc>, _>("at"),
        })).collect::<Vec<_>>(),
    })))
}

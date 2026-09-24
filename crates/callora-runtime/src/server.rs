//! HTTP surface: Twilio webhooks, the media WebSocket, health, metrics and a small
//! read-only admin API.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{OriginalUri, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::json;
use subtle::ConstantTimeEq;
use tokio::sync::{mpsc, oneshot};

use callora_audio::library::VoiceLibrary;
use callora_audio::playout::OutFrame;
use callora_core::business::{is_e164, BusinessRegistry};
use callora_core::customer::Customer;
use callora_core::engine::HandoffSummary;

use crate::ports::{CallInfo, CallRecord, WhisperRegistry};
use crate::session::{Inbound, Services, Session, SessionConfig};
use crate::twilio::{self, StreamMessage};

#[derive(Clone, Debug)]
pub struct ServerSettings {
    /// Public origin Twilio calls, without trailing slash (e.g. https://calls.example.com).
    pub public_base_url: String,
    pub twilio_auth_token: String,
    /// Secrets accepted for stream tokens; the first one signs.
    pub stream_secrets: Vec<String>,
    /// When non-empty, only these callers reach the agent.
    pub allow_list: Vec<String>,
    pub admin_api_key: Option<String>,
    /// Only for local development without Twilio.
    pub skip_signature_validation: bool,
}

pub struct AppState {
    pub registry: BusinessRegistry,
    pub libraries: HashMap<String, Arc<VoiceLibrary>>,
    pub services: Services,
    pub session: SessionConfig,
    pub settings: ServerSettings,
    pub db: Option<sqlx::PgPool>,
    /// What the voice webhook learned, waiting for the media stream of the same call.
    pending: Mutex<HashMap<String, PendingCall>>,
    whispers: Arc<Whispers>,
}

impl AppState {
    pub fn new(
        registry: BusinessRegistry,
        libraries: HashMap<String, Arc<VoiceLibrary>>,
        mut services: Services,
        session: SessionConfig,
        settings: ServerSettings,
        db: Option<sqlx::PgPool>,
    ) -> Arc<Self> {
        let whispers = Arc::new(Whispers { base: settings.public_base_url.clone(), entries: Mutex::new(HashMap::new()) });
        services.whisper = whispers.clone();
        Arc::new(Self { registry, libraries, services, session, settings, db, pending: Mutex::new(HashMap::new()), whispers })
    }
}

struct PendingCall {
    from: Option<String>,
    to: String,
    customer: Option<oneshot::Receiver<Option<Customer>>>,
    at: std::time::Instant,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route(twilio::VOICE_PATH, post(voice))
        .route(twilio::STATUS_PATH, post(call_status))
        .route(twilio::MEDIA_PATH, get(media))
        .route(twilio::WHISPER_PATH, post(whisper))
        .route("/api/businesses", get(api_businesses))
        .route("/api/calls", get(api_calls))
        .route("/api/calls/{id}", get(api_call))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(64 * 1024))
        .with_state(state)
}

fn xml(body: String) -> Response {
    ([(header::CONTENT_TYPE, "text/xml; charset=utf-8")], body).into_response()
}

async fn health(State(s): State<Arc<AppState>>) -> Response {
    let db = match &s.db {
        None => "disabled",
        Some(pool) => match tokio::time::timeout(Duration::from_secs(2), sqlx::query("SELECT 1").execute(pool)).await {
            Ok(Ok(_)) => "ok",
            _ => "down",
        },
    };
    // A down database degrades call history, not calls, so health stays 200.
    Json(json!({ "status": "ok", "businesses": s.registry.len(), "database": db })).into_response()
}

async fn metrics(State(s): State<Arc<AppState>>) -> Response {
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], s.services.metrics.render()).into_response()
}

fn signed(s: &AppState, headers: &HeaderMap, uri: &OriginalUri, params: &BTreeMap<String, String>) -> bool {
    if s.settings.skip_signature_validation {
        return true;
    }
    let Some(provided) = headers.get("x-twilio-signature").and_then(|v| v.to_str().ok()) else { return false };
    let path = uri.0.path_and_query().map_or_else(|| uri.0.path().to_string(), ToString::to_string);
    let url = format!("{}{}", s.settings.public_base_url, path);
    twilio::signature_valid(&s.settings.twilio_auth_token, &url, params, provided)
}

async fn voice(State(s): State<Arc<AppState>>, headers: HeaderMap, uri: OriginalUri, Form(params): Form<BTreeMap<String, String>>) -> Response {
    if !signed(&s, &headers, &uri, &params) {
        tracing::warn!("voice webhook with an invalid Twilio signature");
        return (StatusCode::FORBIDDEN, "invalid signature").into_response();
    }
    let call_sid = params.get("CallSid").cloned().unwrap_or_default();
    let to = params.get("To").cloned().unwrap_or_default();
    let from = params.get("From").cloned().filter(|f| is_e164(f));
    let Some(business) = s.registry.by_number(&to) else {
        tracing::warn!(%to, "call to a number no business answers");
        return xml(twiml_unavailable());
    };
    if !s.settings.allow_list.is_empty() && !from.as_ref().is_some_and(|f| s.settings.allow_list.contains(f)) {
        tracing::info!(call = %call_sid, "caller not on the allow list");
        return xml(twilio::twiml_say_hangup("This number is not available.", "en-US"));
    }

    // Start the customer lookup now: the media stream connects a few hundred ms later,
    // and by then the greeting can already know who is calling.
    let mut customer_rx = None;
    if let (Some(lookup), Some(caller)) = (business.config.customer_lookup.clone(), from.clone()) {
        let (tx, rx) = oneshot::channel();
        customer_rx = Some(rx);
        let runner = s.services.actions.clone();
        let b = business.clone();
        let info = CallInfo { call_id: uuid::Uuid::nil(), call_sid: call_sid.clone(), business_id: b.config.id.clone(), from: Some(caller.clone()), to: to.clone() };
        tokio::spawn(async move {
            let customer = match runner.run(&b, &lookup.action, json!({ "phone": caller }), &info).await {
                Ok(v) => Customer::from_json(&v),
                Err(e) => {
                    tracing::warn!(error = %e, "customer lookup failed");
                    None
                }
            };
            let _ = tx.send(customer);
        });
    }
    {
        let mut pending = s.pending.lock();
        // Forget calls whose media stream never arrived.
        pending.retain(|_, p| p.at.elapsed() < Duration::from_secs(120));
        pending.insert(call_sid.clone(), PendingCall { from: from.clone(), to: to.clone(), customer: customer_rx, at: std::time::Instant::now() });
    }

    let now = chrono::Utc::now().timestamp();
    let secret = s.settings.stream_secrets.first().cloned().unwrap_or_default();
    let token = twilio::create_stream_token(&secret, &call_sid, &business.config.id, 300, now);
    let media_url = format!("{}{}", s.settings.public_base_url.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1), twilio::MEDIA_PATH);
    xml(twilio::twiml_stream(&media_url, &token))
}

fn twiml_unavailable() -> String {
    twilio::twiml_say_hangup("המספר אינו זמין כרגע.", "he-IL")
}

async fn call_status(State(s): State<Arc<AppState>>, headers: HeaderMap, uri: OriginalUri, Form(params): Form<BTreeMap<String, String>>) -> Response {
    if !signed(&s, &headers, &uri, &params) {
        return (StatusCode::FORBIDDEN, "invalid signature").into_response();
    }
    if let (Some(sid), Some(status)) = (params.get("CallSid"), params.get("CallStatus")) {
        s.services.store.record(CallRecord::Status {
            call_sid: sid.clone(),
            status: status.clone(),
            duration_seconds: params.get("CallDuration").and_then(|d| d.parse().ok()),
        });
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn media(State(s): State<Arc<AppState>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| media_socket(s, socket))
}

async fn media_socket(s: Arc<AppState>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    // Wait for `start`; Twilio sends `connected` first.
    let start = loop {
        match tokio::time::timeout(Duration::from_secs(10), stream.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str::<StreamMessage>(&text) {
                Ok(StreamMessage::Start { start }) => break start,
                Ok(_) => continue,
                Err(e) => {
                    tracing::warn!(error = %e, "unparseable media stream message before start");
                    continue;
                }
            },
            Ok(Some(Ok(_))) => continue,
            _ => return,
        }
    };
    let now = chrono::Utc::now().timestamp();
    let token = start.custom_parameters.get("token").map(String::as_str).unwrap_or("");
    let Some(claims) = twilio::verify_stream_token(&s.settings.stream_secrets, token, now) else {
        tracing::warn!(call = %start.call_sid, "media stream rejected: invalid token");
        return;
    };
    if claims.call_sid != start.call_sid {
        tracing::warn!("media stream rejected: token is for another call");
        return;
    }
    let Some(business) = s.registry.by_id(&claims.business_id) else { return };
    let library = s.libraries.get(&business.config.id).cloned().unwrap_or_else(|| Arc::new(VoiceLibrary::empty()));
    let pending = s.pending.lock().remove(&start.call_sid);
    let (from, to, customer) = match pending {
        Some(p) => (p.from, p.to, p.customer),
        None => (None, business.phone_numbers.first().cloned().unwrap_or_default(), None),
    };
    let info = CallInfo { call_id: uuid::Uuid::new_v4(), call_sid: start.call_sid.clone(), business_id: business.config.id.clone(), from, to };

    let (in_tx, in_rx) = mpsc::channel::<Inbound>(256);
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<OutFrame>();
    let stream_sid = start.stream_sid.clone();
    let writer = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            let text = match frame {
                OutFrame::Audio(a) => twilio::media_message(&stream_sid, &a),
                OutFrame::Clear => twilio::clear_message(&stream_sid),
            };
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });
    let reader = tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            let Message::Text(text) = msg else { continue };
            match serde_json::from_str::<StreamMessage>(&text) {
                Ok(StreamMessage::Media { media }) => {
                    if media.track.as_deref().is_none_or(|t| t == "inbound") {
                        if let Some(audio) = media.audio() {
                            let _ = in_tx.try_send(Inbound::Audio(audio));
                        }
                    }
                }
                Ok(StreamMessage::Stop) => break,
                _ => {}
            }
        }
        let _ = in_tx.send(Inbound::Stop).await;
    });

    tracing::info!(call = %info.call_sid, business = %info.business_id, "call connected");
    Session::run(business, library, s.services.clone(), s.session.clone(), info, customer, in_rx, out_tx).await;
    reader.abort();
    writer.abort();
}

// ---------------------------------------------------------------------------------------
// Handoff whisper

pub struct Whispers {
    base: String,
    entries: Mutex<HashMap<String, (String, String, std::time::Instant)>>,
}

impl WhisperRegistry for Whispers {
    fn register(&self, _call: &CallInfo, summary: &HandoffSummary) -> Option<String> {
        if self.base.is_empty() {
            return None;
        }
        let token = hex::encode(rand::random::<[u8; 16]>());
        let mut entries = self.entries.lock();
        entries.retain(|_, (_, _, at)| at.elapsed() < Duration::from_secs(600));
        entries.insert(token.clone(), (summary.text.clone(), "he-IL".into(), std::time::Instant::now()));
        Some(format!("{}{}?t={token}", self.base, twilio::WHISPER_PATH))
    }
}

#[derive(Deserialize)]
struct WhisperQuery {
    t: String,
}

async fn whisper(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: OriginalUri,
    Query(q): Query<WhisperQuery>,
    Form(params): Form<BTreeMap<String, String>>,
) -> Response {
    if !signed(&s, &headers, &uri, &params) {
        return (StatusCode::FORBIDDEN, "invalid signature").into_response();
    }
    let entry = s.whispers.entries.lock().remove(&q.t);
    match entry {
        Some((text, language, _)) => xml(twilio::twiml_say(&text, &language)),
        None => xml(twilio::twiml_say("", "he-IL")),
    }
}

// ---------------------------------------------------------------------------------------
// Admin API (read-only)

fn authorized(s: &AppState, headers: &HeaderMap) -> bool {
    let Some(key) = &s.settings.admin_api_key else { return false };
    headers.get("x-api-key").and_then(|v| v.to_str().ok()).is_some_and(|given| bool::from(given.as_bytes().ct_eq(key.as_bytes())))
}

async fn api_businesses(State(s): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !authorized(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let list: Vec<_> = s
        .registry
        .all()
        .map(|b| {
            let lib = s.libraries.get(&b.config.id).map_or(0, |l| l.len());
            json!({
                "id": b.config.id,
                "name": b.config.name,
                "language": b.config.language,
                "numbers": b.phone_numbers.len(),
                "intents": b.config.intents.iter().map(|i| &i.id).collect::<Vec<_>>(),
                "voice_library_clips": lib,
                "voice_library_expected": callora_core::render::library_entries(b).len(),
                "handoff_configured": b.handoff_number.is_some(),
            })
        })
        .collect();
    Json(list).into_response()
}

#[derive(Deserialize)]
struct CallsQuery {
    business: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

async fn api_calls(State(s): State<Arc<AppState>>, headers: HeaderMap, Query(q): Query<CallsQuery>) -> Response {
    if !authorized(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(pool) = &s.db else { return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response() };
    match crate::store::list_calls(pool, q.business.as_deref(), q.limit.unwrap_or(50).clamp(1, 200), q.offset.unwrap_or(0).max(0)).await {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "listing calls failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn api_call(State(s): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<uuid::Uuid>) -> Response {
    if !authorized(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(pool) = &s.db else { return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response() };
    match crate::store::get_call(pool, id).await {
        Ok(Some(call)) => Json(call).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "reading call failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

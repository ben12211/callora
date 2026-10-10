//! HTTP surface: Twilio webhooks, the media WebSocket, health, metrics, and the owner's
//! pages and admin API (calls, numbers, reviews, orders).

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
    /// Token prices, for the cost per call (`AGENT_PRICES`).
    pub prices: crate::pricing::Prices,
    /// The dashboard's password (`DASHBOARD_PASSWORD`).
    pub dashboard_password: String,
    /// The built dashboard (`web/dist`), served at `/`; none, no site.
    pub web_dir: Option<std::path::PathBuf>,
    /// The WhatsApp service (`WHATSAPP_URL`, `WHATSAPP_TOKEN`); none, no WhatsApp page.
    pub whatsapp: Option<(String, String)>,
    /// Where voice libraries live (`AUDIO_LIBRARY_DIR`); none, no voice switching.
    pub library_dir: Option<std::path::PathBuf>,
    /// The model libraries are made with, when not the business's (`ELEVENLABS_LIBRARY_MODEL`).
    pub library_model: Option<String>,
}

/// A voice library being built in a voice the owner chose.
#[derive(Debug, Clone, serde::Serialize)]
pub struct VoiceBuild {
    pub voice: String,
    /// Empty while it runs; the reason it failed otherwise.
    pub error: Option<String>,
}

pub struct AppState {
    /// The businesses as calls take them: the files with the owner's changes laid over them.
    /// Swapped whole when the owner changes one; a call keeps the business it started with.
    registry: parking_lot::RwLock<Arc<BusinessRegistry>>,
    /// The businesses as their files define them, before the owner's changes.
    files: HashMap<String, Arc<callora_core::business::Business>>,
    /// Each business's voice library. Swapped whole when the owner switches voices: a call
    /// keeps the library (and so the voice) it started with.
    libraries: parking_lot::RwLock<HashMap<String, Arc<VoiceLibrary>>>,
    voice_builds: Mutex<HashMap<String, VoiceBuild>>,
    pub services: Services,
    pub session: SessionConfig,
    pub settings: ServerSettings,
    pub db: Option<sqlx::PgPool>,
    /// What the voice webhook learned, waiting for the media stream of the same call.
    pending: Mutex<HashMap<String, PendingCall>>,
    whispers: Arc<Whispers>,
    sessions: crate::dashboard::Sessions,
    pub whatsapp: Option<Arc<crate::whatsapp::Service>>,
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
        let whispers =
            Arc::new(Whispers { base: settings.public_base_url.clone(), entries: Mutex::new(HashMap::new()) });
        services.whisper = whispers.clone();
        // Sessions are signed with a key only this server has: the stream secret, else the
        // Twilio token.
        let secret = settings.stream_secrets.first().cloned().unwrap_or_else(|| settings.twilio_auth_token.clone());
        let sessions = crate::dashboard::Sessions::new(&settings.dashboard_password, &secret);
        let whatsapp =
            settings.whatsapp.clone().map(|(url, token)| Arc::new(crate::whatsapp::Service::new(url, token)));
        if !settings.public_base_url.is_empty() {
            services.desk =
                Some(Arc::new(crate::desk::Desk::new(settings.public_base_url.clone(), services.telephony.clone())));
        }
        let files = registry.all().map(|b| (b.config.id.clone(), b.clone())).collect();
        Arc::new(Self {
            registry: parking_lot::RwLock::new(Arc::new(registry)),
            files,
            libraries: parking_lot::RwLock::new(libraries),
            voice_builds: Mutex::new(HashMap::new()),
            services,
            session,
            settings,
            db,
            pending: Mutex::new(HashMap::new()),
            whispers,
            sessions,
            whatsapp,
        })
    }
}

impl AppState {
    pub fn registry(&self) -> Arc<BusinessRegistry> {
        self.registry.read().clone()
    }

    /// The business as its file defines it, before the owner's changes.
    pub fn business_file(&self, business_id: &str) -> Option<Arc<callora_core::business::Business>> {
        self.files.get(business_id).cloned()
    }

    /// The business from its file with the owner's `patch` laid over it, checked as a file
    /// is, and put in use for new calls. The problems found when it is refused.
    pub fn apply_config(&self, business_id: &str, patch: &serde_json::Value) -> Result<(), Vec<String>> {
        let file = self.business_file(business_id).ok_or_else(|| vec!["unknown business".to_string()])?;
        let mut config = serde_json::to_value(&file.config).map_err(|e| vec![e.to_string()])?;
        crate::owner_config::merge(&mut config, patch);
        let env = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let business = callora_core::business::Business::from_json(&config.to_string(), "dashboard", &env).map_err(
            |e| match e {
                callora_core::business::LoadError::Invalid { issues, .. } => {
                    issues.iter().map(|i| format!("{}: {}", i.path, i.message)).collect()
                }
                other => vec![other.to_string()],
            },
        )?;
        let next = self.registry().with(business).map_err(|e| vec![e.to_string()])?;
        *self.registry.write() = Arc::new(next);
        Ok(())
    }

    /// At start: each business with the owner's saved changes over its file. Returns the
    /// businesses whose changes need recording (a changed sentence or pace).
    pub fn apply_saved_configs(self: &Arc<Self>) -> Vec<String> {
        let mut to_record = Vec::new();
        for id in self.files.keys().cloned().collect::<Vec<_>>() {
            let Some(patch) = self.services.settings.config_patch(&id) else { continue };
            let changed = crate::owner_config::changed(&patch);
            match self.apply_config(&id, &patch) {
                Ok(()) => {
                    tracing::info!(business = %id, ?changed, "the owner's changes are in use");
                    if changed.iter().any(|p| crate::owner_config::needs_recording(p)) {
                        to_record.push(id);
                    }
                }
                Err(problems) => {
                    tracing::error!(business = %id, ?problems, "the owner's saved changes no longer fit the file; the file is used")
                }
            }
        }
        to_record
    }

    /// Records what the business's current settings say that its voice library lacks (a
    /// changed sentence, a new pace), in the voice in use. False when a recording runs.
    pub fn record_changes(self: &Arc<Self>, business_id: &str) -> bool {
        let Some(b) = self.registry().by_id(business_id) else { return false };
        let Some(voice) = self.active_voice(&b) else { return false };
        self.switch_voice(business_id, voice)
    }

    /// The business's voice library now.
    pub fn library(&self, business_id: &str) -> Arc<VoiceLibrary> {
        self.libraries.read().get(business_id).cloned().unwrap_or_else(|| Arc::new(VoiceLibrary::empty()))
    }

    /// The voice of the library in use (the business's own voice when there is none).
    pub fn active_voice(&self, business: &callora_core::business::Business) -> Option<String> {
        self.library(&business.config.id).voice_id.clone().or_else(|| business.voice_id.clone())
    }

    pub fn voice_build(&self, business_id: &str) -> Option<VoiceBuild> {
        self.voice_builds.lock().get(business_id).cloned()
    }

    /// Start using `voice`: its library is built (only what is missing), then swapped in.
    /// Until then calls keep the voice they have, so no call mixes two voices. Returns
    /// false when a build for this business is already running.
    pub fn switch_voice(self: &Arc<Self>, business_id: &str, voice: String) -> bool {
        {
            let mut builds = self.voice_builds.lock();
            if builds.get(business_id).is_some_and(|b| b.error.is_none()) {
                return false;
            }
            builds.insert(business_id.to_string(), VoiceBuild { voice: voice.clone(), error: None });
        }
        let state = self.clone();
        let id = business_id.to_string();
        tokio::spawn(async move {
            let result = state.build_voice(&id, &voice).await;
            let mut builds = state.voice_builds.lock();
            match result {
                Ok(()) => {
                    builds.remove(&id);
                }
                Err(e) => {
                    tracing::error!(business = %id, voice = %voice, error = %format!("{e:#}"), "switching voices failed; the voice in use stays");
                    builds.insert(id, VoiceBuild { voice, error: Some(format!("{e:#}")) });
                }
            }
        });
        true
    }

    async fn build_voice(&self, business_id: &str, voice: &str) -> anyhow::Result<()> {
        let business = self.registry().by_id(business_id).ok_or_else(|| anyhow::anyhow!("unknown business"))?;
        let root = self.settings.library_dir.clone().ok_or_else(|| anyhow::anyhow!("AUDIO_LIBRARY_DIR is not set"))?;
        let tts = self.services.tts.clone().ok_or_else(|| anyhow::anyhow!("no text-to-speech configured"))?;
        let root = callora_audio::library::voice_root(&root, voice, business.voice_id.as_deref());
        let model = self.settings.library_model.clone().unwrap_or_else(|| business.config.voice.library_model.clone());
        tracing::info!(business = %business_id, %voice, path = %root.display(), "building the voice library for the chosen voice");
        let report = callora_audio::library::LibraryBuilder {
            business: &business,
            synthesizer: tts,
            voice_id: voice.to_string(),
            model,
            root: root.clone(),
            concurrency: 4,
        }
        .build()
        .await?;
        tracing::info!(
            business = %business_id,
            %voice,
            generated = report.generated,
            reused = report.reused,
            failed = report.failed.len(),
            "voice library built"
        );
        let library = VoiceLibrary::load_for(&root, &business, Some(voice))?;
        // Most of it missing would play almost everything through live TTS: keep the old voice.
        if library.len() * 10 < report.total * 9 {
            anyhow::bail!("only {} of {} sentences were recorded", library.len(), report.total);
        }
        self.libraries.write().insert(business_id.to_string(), Arc::new(library));
        tracing::info!(business = %business_id, %voice, "the new voice is in use for new calls");
        Ok(())
    }

    /// At start: each business's chosen voice, when it is not the one loaded. Its library is
    /// used at once if it was built before, and completed in the background.
    pub fn apply_saved_voices(self: &Arc<Self>) {
        let Some(root) = self.settings.library_dir.clone() else { return };
        for b in self.registry().all() {
            let Some(voice) = self.services.settings.voice(&b.config.id) else { continue };
            if self.active_voice(b).as_deref() == Some(voice.as_str()) {
                continue;
            }
            let dir = callora_audio::library::voice_root(&root, &voice, b.voice_id.as_deref());
            if let Ok(lib) = VoiceLibrary::load_for(&dir, b, Some(&voice)) {
                if !lib.is_empty() {
                    tracing::info!(business = %b.config.id, %voice, clips = lib.len(), "the chosen voice is in use");
                    self.libraries.write().insert(b.config.id.clone(), Arc::new(lib));
                }
            }
            self.switch_voice(&b.config.id, voice);
        }
    }
}

/// The customer record with the name of the caller's last ride from our own orders, when
/// the record has none: a known caller is not asked for it again (it is never said). Nothing
/// else of that ride: told the agent, its addresses filled in a garbled answer ("עזרא 11",
/// heard "עשרה, אחד עשרה", was booked as the last ride's "רבי אליעזר 11").
fn with_last_ride(
    customer: Option<Customer>,
    last: Option<(serde_json::Value, chrono::DateTime<chrono::Utc>)>,
) -> Option<Customer> {
    let customer = customer.map(|mut c| {
        c.name = c.name.filter(|n| plausible_name(n));
        c
    });
    let Some((card, _)) = last else { return customer };
    let name = card["details"].as_array().and_then(|details| {
        details
            .iter()
            .find(|d| d["field"] == "customer_name")
            .and_then(|d| d["value"].as_str())
            .map(str::trim)
            .filter(|v| plausible_name(v))
            .map(str::to_string)
    });
    let Some(name) = name else { return customer };
    let mut c = customer.unwrap_or_default();
    if c.name.is_none() {
        c.name = Some(name);
    }
    Some(c)
}

/// A name a known caller can be booked under without being asked: a few words, none twice.
/// Recognition once saved "דוד יוסף חיים משה יוסף חיים דוד משה", and every later ride of that
/// caller went out under it, never asked.
fn plausible_name(name: &str) -> bool {
    let words: Vec<&str> = name.split_whitespace().collect();
    let mut seen = std::collections::HashSet::new();
    (1..=4).contains(&words.len())
        && words.iter().all(|w| seen.insert(*w))
        && name.chars().all(|c| c.is_alphabetic() || c.is_whitespace() || matches!(c, '-' | '\'' | '"' | '״' | '׳'))
}

struct PendingCall {
    from: Option<String>,
    to: String,
    customer: Option<oneshot::Receiver<Option<Customer>>>,
    at: std::time::Instant,
}

/// Audio missing from the caller's stream for this long is the line cutting out.
const LINE_GAP_MS: u64 = 100;

pub fn router(state: Arc<AppState>) -> Router {
    let web_dir = state.settings.web_dir.clone();
    let app = Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .route(twilio::VOICE_PATH, post(voice))
        .route(twilio::STATUS_PATH, post(call_status))
        .route(twilio::MEDIA_PATH, get(media))
        .route(twilio::WHISPER_PATH, post(whisper))
        .route(twilio::DESK_PATH, post(desk_answered))
        .route(twilio::DESK_STATUS_PATH, post(desk_status))
        .route(twilio::DESK_CONFERENCE_PATH, post(desk_conference))
        .route("/api/settings", get(api_settings))
        .route("/api/settings/{business}", axum::routing::put(api_save_settings))
        .route("/api/settings/{business}/price-bot", axum::routing::put(api_save_price_bot))
        .route("/api/settings/{business}/voice", axum::routing::put(api_save_voice))
        .route("/api/settings/agent-model", axum::routing::put(api_save_agent_model))
        .route("/api/config/{business}", get(api_config).put(api_save_config))
        .route("/api/access", get(api_access).put(api_save_access))
        .route("/api/blocked", get(api_blocked))
        .route("/api/blocked/{number}", axum::routing::put(api_block))
        .route("/api/settings/{business}/price-bot/test", post(api_test_price_bot))
        .route("/api/businesses", get(api_businesses))
        .route("/api/calls", get(api_calls))
        .route("/api/orders", get(api_orders))
        .route("/api/login", post(api_login))
        .route("/api/logout", post(api_logout))
        .route("/api/session", get(api_session))
        .route("/api/stats", get(api_stats))
        .route("/api/stats/daily", get(api_stats_daily))
        .route("/api/calls/{id}", get(api_call))
        .route("/api/calls/{id}/review", axum::routing::put(api_review))
        .route("/api/calls/{id}/eval-case", get(api_eval_case))
        .route("/api/utterances/{id}", get(api_utterance))
        .merge(crate::whatsapp::routes())
        .route("/api/{*rest}", axum::routing::any(|| async { StatusCode::NOT_FOUND }))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(64 * 1024))
        .with_state(state);
    // The dashboard: its files, and index.html for its own paths ("/calls/…"), so a reload
    // or a link opens the page. Never cached stale: a deploy changes the file names.
    match web_dir {
        Some(dir) => {
            let site = tower_http::services::ServeDir::new(&dir)
                .fallback(tower_http::services::ServeFile::new(dir.join("index.html")));
            app.fallback_service(tower_http::set_header::SetResponseHeader::if_not_present(
                site,
                header::CACHE_CONTROL,
                header::HeaderValue::from_static("no-cache"),
            ))
        }
        None => app,
    }
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
    Json(json!({ "status": "ok", "businesses": s.registry().len(), "database": db })).into_response()
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

async fn voice(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: OriginalUri,
    Form(params): Form<BTreeMap<String, String>>,
) -> Response {
    if !signed(&s, &headers, &uri, &params) {
        tracing::warn!("voice webhook with an invalid Twilio signature");
        return (StatusCode::FORBIDDEN, "invalid signature").into_response();
    }
    let call_sid = params.get("CallSid").cloned().unwrap_or_default();
    let to = params.get("To").cloned().unwrap_or_default();
    let from = params.get("From").cloned().filter(|f| is_e164(f));
    let Some(business) = s.registry().by_number(&to) else {
        tracing::warn!(%to, "call to a number no business answers");
        return xml(twiml_unavailable());
    };
    if !s.services.settings.access(&s.settings.allow_list).allows(from.as_deref()) {
        tracing::info!(call = %call_sid, "development: the caller is not on the access list");
        return xml(twiml_unavailable());
    }
    if from.as_deref().is_some_and(|f| s.services.settings.is_blocked(f)) {
        tracing::info!(call = %call_sid, "a blocked number; rejected");
        return xml(twilio::twiml_reject());
    }

    // Start the customer lookup now: the media stream connects a few hundred ms later,
    // and by then the greeting can already know who is calling.
    let mut customer_rx = None;
    if let (Some(lookup), Some(caller)) = (business.config.customer_lookup.clone(), from.clone()) {
        let (tx, rx) = oneshot::channel();
        customer_rx = Some(rx);
        let runner = s.services.actions.clone();
        let b = business.clone();
        let db = s.db.clone();
        let info = CallInfo {
            call_id: uuid::Uuid::nil(),
            call_sid: call_sid.clone(),
            business_id: b.config.id.clone(),
            from: Some(caller.clone()),
            to: to.clone(),
        };
        tokio::spawn(async move {
            let (looked_up, last) =
                tokio::join!(runner.run(&b, &lookup.action, json!({ "phone": caller }), &info), async {
                    match &db {
                        Some(pool) => crate::store::last_ride(pool, &b.config.id, &caller).await.ok().flatten(),
                        None => None,
                    }
                });
            let customer = match looked_up {
                Ok(v) => Customer::from_json(&v),
                Err(e) => {
                    tracing::warn!(error = %e, "customer lookup failed");
                    None
                }
            };
            let _ = tx.send(with_last_ride(customer, last));
        });
    }
    {
        let mut pending = s.pending.lock();
        // Forget calls whose media stream never arrived.
        pending.retain(|_, p| p.at.elapsed() < Duration::from_secs(120));
        pending.insert(
            call_sid.clone(),
            PendingCall { from: from.clone(), to: to.clone(), customer: customer_rx, at: std::time::Instant::now() },
        );
    }

    let now = chrono::Utc::now().timestamp();
    let secret = s.settings.stream_secrets.first().cloned().unwrap_or_default();
    let token = twilio::create_stream_token(&secret, &call_sid, &business.config.id, 300, now);
    let media_url = format!(
        "{}{}",
        s.settings.public_base_url.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1),
        twilio::MEDIA_PATH
    );
    xml(twilio::twiml_stream(&media_url, &token))
}

fn twiml_unavailable() -> String {
    twilio::twiml_say_hangup("המספר אינו זמין כרגע.", "he-IL")
}

async fn call_status(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: OriginalUri,
    Form(params): Form<BTreeMap<String, String>>,
) -> Response {
    if !signed(&s, &headers, &uri, &params) {
        return (StatusCode::FORBIDDEN, "invalid signature").into_response();
    }
    if let (Some(sid), Some(status)) = (params.get("CallSid"), params.get("CallStatus")) {
        if matches!(status.as_str(), "completed" | "canceled" | "failed" | "busy" | "no-answer") {
            if let Some(desk) = &s.services.desk {
                desk.caller_ended(sid).await;
            }
        }
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
    let Some(business) = s.registry().by_id(&claims.business_id) else { return };
    let library = s.library(&business.config.id);
    let pending = s.pending.lock().remove(&start.call_sid);
    let (from, to, customer) = match pending {
        Some(p) => (p.from, p.to, p.customer),
        None => (None, business.phone_numbers.first().cloned().unwrap_or_default(), None),
    };
    let info = CallInfo {
        call_id: uuid::Uuid::new_v4(),
        call_sid: start.call_sid.clone(),
        business_id: business.config.id.clone(),
        from,
        to,
    };

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
        // Where the next frame should start: audio missing before a frame is the line cutting out.
        let mut next_at: Option<u64> = None;
        while let Some(Ok(msg)) = stream.next().await {
            let Message::Text(text) = msg else { continue };
            match serde_json::from_str::<StreamMessage>(&text) {
                Ok(StreamMessage::Media { media }) if media.track.as_deref().is_none_or(|t| t == "inbound") => {
                    if let Some(audio) = media.audio() {
                        if let Some(at) = media.timestamp_ms() {
                            if let Some(gap) = next_at.map(|n| at.saturating_sub(n)).filter(|g| *g >= LINE_GAP_MS) {
                                let _ = in_tx.try_send(Inbound::Gap(gap));
                            }
                            next_at = Some(at + audio.len() as u64 / 8);
                        }
                        let _ = in_tx.try_send(Inbound::Audio(audio));
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

/// A desk number answered a transfer: the first one takes the caller.
async fn desk_answered(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: OriginalUri,
    Query(q): Query<WhisperQuery>,
    Form(params): Form<BTreeMap<String, String>>,
) -> Response {
    if !signed(&s, &headers, &uri, &params) {
        return (StatusCode::FORBIDDEN, "invalid signature").into_response();
    }
    let leg = params.get("CallSid").map(String::as_str).unwrap_or("");
    match &s.services.desk {
        Some(desk) => xml(desk.answered(&q.t, leg)),
        None => xml(twilio::twiml_hangup()),
    }
}

/// How a desk call ended: when the last desk number refuses, the caller is told at once.
async fn desk_status(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: OriginalUri,
    Query(q): Query<WhisperQuery>,
    Form(params): Form<BTreeMap<String, String>>,
) -> Response {
    if !signed(&s, &headers, &uri, &params) {
        return (StatusCode::FORBIDDEN, "invalid signature").into_response();
    }
    let leg = params.get("CallSid").map(String::as_str).unwrap_or("");
    let status = params.get("CallStatus").map(String::as_str).unwrap_or("");
    if let Some(desk) = &s.services.desk {
        desk.leg_ended(&q.t, leg, status).await;
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn desk_conference(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    uri: OriginalUri,
    Query(q): Query<WhisperQuery>,
    Form(params): Form<BTreeMap<String, String>>,
) -> Response {
    if !signed(&s, &headers, &uri, &params) {
        return (StatusCode::FORBIDDEN, "invalid signature").into_response();
    }
    if let Some(desk) = &s.services.desk {
        desk.conference_event(&q.t, params.get("StatusCallbackEvent").map(String::as_str).unwrap_or("")).await;
    }
    StatusCode::NO_CONTENT.into_response()
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
// Admin API (`X-Api-Key`, or the dashboard's session cookie): read-only, except the
// owner's review of a call

pub(crate) fn authorized(s: &AppState, headers: &HeaderMap) -> bool {
    let by_key = s.settings.admin_api_key.as_ref().is_some_and(|key| {
        headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|given| bool::from(given.as_bytes().ct_eq(key.as_bytes())))
    });
    let by_session = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .filter_map(crate::dashboard::cookie_token)
        .any(|token| s.sessions.valid(token, chrono::Utc::now().timestamp()));
    by_key || by_session
}

/// Who is trying a password: the address Caddy saw, else the direct peer.
fn client_address(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "direct".into())
}

#[derive(Deserialize)]
struct LoginBody {
    password: String,
}

async fn api_login(State(s): State<Arc<AppState>>, headers: HeaderMap, Json(body): Json<LoginBody>) -> Response {
    let client = client_address(&headers);
    if !s.sessions.allowed(&client) {
        return (StatusCode::TOO_MANY_REQUESTS, Json(json!({ "error": "too_many_attempts" }))).into_response();
    }
    if !s.sessions.password_matches(&body.password) {
        s.sessions.failed(&client);
        tracing::warn!(%client, "dashboard login failed");
        // Each wrong guess costs time.
        tokio::time::sleep(Duration::from_millis(400)).await;
        return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "wrong_password" }))).into_response();
    }
    s.sessions.succeeded(&client);
    tracing::info!(%client, "dashboard login");
    let token = s.sessions.issue(chrono::Utc::now().timestamp());
    (StatusCode::NO_CONTENT, [(header::SET_COOKIE, crate::dashboard::set_cookie(&token))]).into_response()
}

async fn api_logout() -> Response {
    (StatusCode::NO_CONTENT, [(header::SET_COOKIE, crate::dashboard::clear_cookie())]).into_response()
}

/// Whether the browser is signed in, and what the dashboard shows about the business.
async fn api_session(State(s): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !authorized(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let businesses: Vec<_> = s.registry().all().map(|b| json!({ "id": b.config.id, "name": b.config.name })).collect();
    Json(json!({ "businesses": businesses, "database": s.db.is_some() })).into_response()
}

#[derive(Deserialize)]
struct DailyQuery {
    business: Option<String>,
    days: Option<i32>,
}

async fn api_stats_daily(State(s): State<Arc<AppState>>, headers: HeaderMap, Query(q): Query<DailyQuery>) -> Response {
    if !authorized(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(pool) = &s.db else { return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response() };
    let days = q.days.unwrap_or(30).clamp(1, 365);
    match crate::store::daily(pool, q.business.as_deref(), days).await {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "daily numbers failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn api_businesses(State(s): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !authorized(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let list: Vec<_> = s
        .registry()
        .all()
        .map(|b| {
            let lib = s.library(&b.config.id).len();
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
    match crate::store::list_calls(
        pool,
        q.business.as_deref(),
        q.limit.unwrap_or(50).clamp(1, 200),
        q.offset.unwrap_or(0).max(0),
    )
    .await
    {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "listing calls failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn api_orders(State(s): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !authorized(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(pool) = &s.db else { return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response() };
    match crate::store::list_orders(pool, 100).await {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "listing orders failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn api_settings(State(s): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !authorized(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let businesses: Vec<_> = s
        .registry()
        .all()
        .map(|b| {
            json!({
                "id": b.config.id,
                "name": b.config.name,
                "desk": s.services.settings.desk(b),
                "price_bot": s.services.settings.price_bot(&b.config.id),
                "voice": {
                    "active": s.active_voice(b),
                    "chosen": s.services.settings.voice(&b.config.id).or_else(|| b.voice_id.clone()),
                    "default": b.voice_id,
                    "choices": b.config.voice.choices,
                    "building": s.voice_build(&b.config.id),
                    "clips": s.library(&b.config.id).len(),
                },
            })
        })
        .collect();
    Json(json!({
        "businesses": businesses,
        "music": crate::settings::DeskSettings::music_names(),
        "saving": s.db.is_some(),
        "transfers": s.services.desk.is_some(),
        "whatsapp": s.whatsapp.is_some(),
        "voice_switching": s.settings.library_dir.is_some() && s.services.tts.is_some(),
        "agent": s.services.settings.agent_control().map(|c| c.view()),
    }))
    .into_response()
}

/// Change the agent's model: tried with a small request, then used for the calls' next turns and
/// saved, so it is still in use after a restart. `null` goes back to the environment's model.
async fn api_save_agent_model(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(change): Json<Option<crate::agent_model::AgentModelSettings>>,
) -> Response {
    if !authorized(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(control) = s.services.settings.agent_control() else {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "problems": ["אין בשרת מפתח למודל של הסוכן"] })))
            .into_response();
    };
    let Some(pool) = &s.db else { return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response() };
    let change = change.map(crate::agent_model::AgentModelSettings::normalized);
    let wanted = change.clone().unwrap_or_else(|| control.defaults().clone());
    if let Err(problems) = control.switch(wanted).await {
        return (StatusCode::BAD_REQUEST, Json(json!({ "problems": problems }))).into_response();
    }
    // A choice equal to the default is no choice: a new default then reaches it.
    let saved = change.filter(|c| c != control.defaults());
    if let Err(e) = s.services.settings.save_agent_model(pool, saved).await {
        tracing::error!(error = %e, "saving the agent's model failed; it is in use until the next restart");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    Json(control.view()).into_response()
}

#[derive(serde::Deserialize)]
struct VoiceChange {
    voice_id: String,
}

/// Switch a business to another voice from its list: saved, then built and swapped in.
/// The behavior page: each editable item as the file has it and as it is now, which ones the
/// owner changed, and the agent's whole system prompt as the model gets it.
async fn api_config(State(s): State<Arc<AppState>>, headers: HeaderMap, Path(business): Path<String>) -> Response {
    if !authorized(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let (Some(file), Some(current)) = (s.business_file(&business), s.registry().by_id(&business)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    config_view(&s, &file, &current)
}

fn config_view(
    s: &AppState,
    file: &callora_core::business::Business,
    current: &callora_core::business::Business,
) -> Response {
    let as_json = |b: &callora_core::business::Business| serde_json::to_value(&b.config).unwrap_or_default();
    let patch = s.services.settings.config_patch(&current.config.id).unwrap_or_else(|| json!({}));
    Json(json!({
        "file": crate::owner_config::view(&as_json(file)),
        "current": crate::owner_config::view(&as_json(current)),
        "changed": crate::owner_config::changed(&patch),
        "prompt": current.config.agent.as_ref().map(|_| callora_core::agent::system_prompt(current)),
        "saving": s.db.is_some(),
        "recording": s.voice_build(&current.config.id).is_some_and(|v| v.error.is_none()),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct ConfigChange {
    path: String,
    /// The new value; absent or null goes back to the file's.
    #[serde(default)]
    value: Option<serde_json::Value>,
}

/// One item changed (or reset) on the behavior page: checked as the file would be, used by new
/// calls at once, kept across restarts. A changed sentence is recorded in the voice in use.
async fn api_save_config(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(business): Path<String>,
    Json(change): Json<ConfigChange>,
) -> Response {
    if !authorized(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !crate::owner_config::editable(&change.path) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "problems": ["אי אפשר לשנות את זה מכאן"] }))).into_response();
    }
    let Some(file) = s.business_file(&business) else { return StatusCode::NOT_FOUND.into_response() };
    let Some(pool) = &s.db else { return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response() };
    let mut patch = s.services.settings.config_patch(&business).unwrap_or_else(|| json!({}));
    let value = change.value.filter(|v| !v.is_null());
    crate::owner_config::set(&mut patch, &change.path, value);
    if let Err(problems) = s.apply_config(&business, &patch) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "problems": problems }))).into_response();
    }
    if let Err(e) = s.services.settings.save_config_patch(pool, &business, patch).await {
        tracing::error!(error = %e, "saving the owner's changes failed");
        // Back to what is saved, so what calls use is what a restart would.
        let saved = s.services.settings.config_patch(&business).unwrap_or_else(|| json!({}));
        let _ = s.apply_config(&business, &saved);
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    tracing::info!(business = %business, path = %change.path, "the owner changed the configuration");
    if crate::owner_config::needs_recording(&change.path) {
        s.record_changes(&business);
    }
    let Some(current) = s.registry().by_id(&business) else { return StatusCode::NOT_FOUND.into_response() };
    config_view(&s, &file, &current)
}

/// Who may call: development (the access list only) or production (anyone).
async fn api_access(State(s): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !authorized(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    access_view(&s)
}

fn access_view(s: &AppState) -> Response {
    let access = s.services.settings.access(&s.settings.allow_list);
    Json(json!({
        "mode": access.mode,
        "numbers": access.numbers,
        "from_deployment": !s.services.settings.access_chosen(),
        "saving": s.db.is_some(),
    }))
    .into_response()
}

async fn api_save_access(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(access): Json<crate::settings::Access>,
) -> Response {
    if !authorized(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let access = access.normalized();
    let problems = access.problems();
    if !problems.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "problems": problems }))).into_response();
    }
    let Some(pool) = &s.db else { return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response() };
    tracing::info!(mode = ?access.mode, numbers = access.numbers.len(), "the owner changed who may call");
    if let Err(e) = s.services.settings.save_access(pool, access).await {
        tracing::error!(error = %e, "saving who may call failed");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    access_view(&s)
}

async fn api_blocked(State(s): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if !authorized(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(json!({ "numbers": s.services.settings.blocked_numbers() })).into_response()
}

#[derive(Deserialize)]
struct BlockChange {
    blocked: bool,
}

/// Blocks a caller's number (a prank caller, from the call page), or unblocks it.
async fn api_block(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(number): Path<String>,
    Json(change): Json<BlockChange>,
) -> Response {
    if !authorized(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let number = number.trim().to_string();
    if !is_e164(&number) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "problems": ["מספר לא תקין"] }))).into_response();
    }
    let Some(pool) = &s.db else { return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response() };
    if let Err(e) = s.services.settings.set_blocked(pool, &number, change.blocked).await {
        tracing::error!(error = %e, "saving the blocked numbers failed");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    Json(json!({ "numbers": s.services.settings.blocked_numbers() })).into_response()
}

async fn api_save_voice(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(business): Path<String>,
    Json(change): Json<VoiceChange>,
) -> Response {
    if !authorized(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(b) = s.registry().by_id(&business) else { return StatusCode::NOT_FOUND.into_response() };
    let voice = change.voice_id.trim().to_string();
    let known = b.config.voice.choices.iter().any(|c| c.id == voice) || b.voice_id.as_deref() == Some(voice.as_str());
    if !known {
        return (StatusCode::BAD_REQUEST, Json(json!({ "problems": ["הקול לא ברשימה"] }))).into_response();
    }
    if s.settings.library_dir.is_none() || s.services.tts.is_none() {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "problems": ["החלפת קול לא זמינה בשרת הזה"] })))
            .into_response();
    }
    let Some(pool) = &s.db else { return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response() };
    if s.voice_build(&business).is_some_and(|v| v.error.is_none()) {
        return (StatusCode::CONFLICT, Json(json!({ "problems": ["כבר מכינים קול אחר; אפשר לנסות שוב בעוד דקה"] })))
            .into_response();
    }
    // The business's own voice is saved as "no choice", so a new default reaches it.
    let saved = (b.voice_id.as_deref() != Some(voice.as_str())).then(|| voice.clone());
    if let Err(e) = s.services.settings.save_voice(pool, &business, saved).await {
        tracing::error!(error = %e, "saving the voice failed");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if s.active_voice(&b).as_deref() != Some(voice.as_str()) {
        s.switch_voice(&business, voice.clone());
    }
    Json(json!({ "chosen": voice, "building": s.voice_build(&business) })).into_response()
}

async fn api_save_price_bot(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(business): Path<String>,
    Json(bot): Json<Option<crate::settings::PriceBotSettings>>,
) -> Response {
    if !authorized(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if s.registry().by_id(&business).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(pool) = &s.db else { return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response() };
    let bot = bot.filter(|b| !b.account.trim().is_empty() && !b.chat_id.trim().is_empty());
    match s.services.settings.save_price_bot(pool, &business, bot.clone()).await {
        Ok(()) => Json(bot).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "saving the price bot failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(serde::Deserialize)]
struct PriceTest {
    from: String,
    to: String,
    #[serde(default)]
    passengers: Option<u32>,
}

/// The price of a route, as a call would get it: the question sent, the bot's answer and the
/// quote read from it.
async fn api_test_price_bot(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(business): Path<String>,
    Json(t): Json<PriceTest>,
) -> Response {
    if !authorized(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(b) = s.registry().by_id(&business) else { return StatusCode::NOT_FOUND.into_response() };
    let Some(action) =
        b.config.actions.iter().find(|(_, a)| {
            a.backends.iter().any(|k| matches!(k, callora_core::config::ActionBackend::PriceBot { .. }))
        })
    else {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "לעסק אין פעולת מחיר מבוט" }))).into_response();
    };
    let mut slots = json!({ "price_from": { "spoken": t.from }, "price_to": { "spoken": t.to } });
    if let Some(p) = t.passengers {
        slots["passengers"] = json!(p);
    }
    let call = crate::ports::CallInfo {
        call_id: uuid::Uuid::new_v4(),
        call_sid: "dashboard-test".into(),
        from: None,
        to: String::new(),
        business_id: b.config.id.clone(),
    };
    let started = std::time::Instant::now();
    let result = s.services.actions.run(&b, action.0, json!({ "run_id": 0, "slots": slots }), &call).await;
    let ms = started.elapsed().as_millis() as u64;
    match result {
        Ok(quote) => Json(json!({ "ok": true, "ms": ms, "quote": quote })).into_response(),
        Err(e) => Json(json!({ "ok": false, "ms": ms, "error": e.error })).into_response(),
    }
}

async fn api_save_settings(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(business): Path<String>,
    Json(mut desk): Json<crate::settings::DeskSettings>,
) -> Response {
    if !authorized(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if s.registry().by_id(&business).is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(pool) = &s.db else { return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response() };
    // "050-123 4567" as typed → "+972501234567".
    desk.numbers = desk.numbers.iter().map(|n| normalize_phone(n)).filter(|n| !n.is_empty()).collect();
    let problems = desk.problems();
    if !problems.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "problems": problems }))).into_response();
    }
    match s.services.settings.save_desk(pool, &business, desk.clone()).await {
        Ok(()) => Json(desk).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "saving settings failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// An Israeli number as people type it, in E.164.
fn normalize_phone(raw: &str) -> String {
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit() || *c == '+').collect();
    if let Some(rest) = digits.strip_prefix("00") {
        return format!("+{rest}");
    }
    if let Some(rest) = digits.strip_prefix('0') {
        return format!("+972{rest}");
    }
    digits
}

#[derive(Deserialize)]
struct StatsQuery {
    business: Option<String>,
    days: Option<i32>,
}

async fn api_stats(State(s): State<Arc<AppState>>, headers: HeaderMap, Query(q): Query<StatsQuery>) -> Response {
    if !authorized(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(pool) = &s.db else { return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response() };
    let days = q.days.unwrap_or(7).clamp(1, 365);
    match crate::store::call_facts(pool, q.business.as_deref(), days).await {
        Ok(facts) => {
            let mut stats = crate::review::stats(&facts, &s.settings.prices);
            stats["days"] = json!(days);
            Json(stats).into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "call numbers failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[derive(Deserialize)]
struct ReviewBody {
    verdict: String,
    #[serde(default)]
    note: String,
}

async fn api_review(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<uuid::Uuid>,
    Json(body): Json<ReviewBody>,
) -> Response {
    if !authorized(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(pool) = &s.db else { return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response() };
    if body.verdict != "good" && body.verdict != "bad" {
        return (StatusCode::BAD_REQUEST, "verdict is good or bad").into_response();
    }
    let note: String = body.note.chars().take(2000).collect();
    match crate::store::set_review(pool, id, &body.verdict, note.trim()).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "storing a review failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn api_eval_case(State(s): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<uuid::Uuid>) -> Response {
    if !authorized(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(pool) = &s.db else { return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response() };
    match crate::store::get_call(pool, id).await {
        Ok(Some(call)) => Json(crate::review::eval_case(&call)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "reading call failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// A recorded utterance (sampled numbers only), as WAV for the browser.
async fn api_utterance(State(s): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<i64>) -> Response {
    if !authorized(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(pool) = &s.db else { return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response() };
    match crate::store::utterance_audio(pool, id).await {
        Ok(Some(audio)) => (
            [(header::CONTENT_TYPE, "audio/wav"), (header::CACHE_CONTROL, "private, max-age=3600")],
            callora_audio::mulaw::to_wav(&audio),
        )
            .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, "reading an utterance failed");
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

#[cfg(test)]
mod last_ride_tests {
    use super::*;

    #[test]
    fn a_last_ride_gives_only_its_name() {
        let card = json!({
            "details": [
                { "field": "pickup", "value": "בן זכאי 40, אלעד", "address": "רבן יוחנן בן זכאי 40, אלעד" },
                { "field": "destination", "value": "סוכות 12, ירושלים", "address": null },
                { "field": "passengers", "value": "3" },
                { "field": "customer_name", "value": "דוד" }
            ],
            "result": { "ride_id": "R-1" }
        });
        let c = with_last_ride(None, Some((card, chrono::Utc::now()))).expect("a customer");
        assert_eq!(c.name.as_deref(), Some("דוד"));
        assert!(c.data.is_empty() && c.places.is_empty(), "no addresses of the last ride");
        assert!(with_last_ride(None, None).is_none(), "no ride, no customer");
    }

    #[test]
    fn a_garbled_name_is_not_used_the_caller_is_asked() {
        let card = |name: &str| json!({ "details": [{ "field": "customer_name", "value": name }] });
        let garbled = "דוד יוסף חיים משה יוסף חיים דוד משה";
        let c = with_last_ride(None, Some((card(garbled), chrono::Utc::now()))).unwrap_or_default();
        assert!(c.name.is_none(), "the call of 13:36 booked its ride under this");
        let saved = Customer { name: Some(garbled.into()), ..Default::default() };
        assert!(with_last_ride(Some(saved), None).and_then(|c| c.name).is_none(), "nor from the record");
        for name in ["דוד", "יוסי כהן", "בן-דוד", "רבקה בת שבע לוי"] {
            let c = with_last_ride(None, Some((card(name), chrono::Utc::now()))).expect("a customer");
            assert_eq!(c.name.as_deref(), Some(name));
        }
        assert!(with_last_ride(None, Some((card("דוד 45"), chrono::Utc::now()))).and_then(|c| c.name).is_none());
    }
}

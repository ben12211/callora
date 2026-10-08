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
    /// The ElevenLabs API key and base URL, for handing calls to an ElevenLabs agent; none, no
    /// such option on the settings page.
    pub eleven_agents: Option<(String, Option<String>)>,
}

/// A voice library being built in a voice the owner chose.
#[derive(Debug, Clone, serde::Serialize)]
pub struct VoiceBuild {
    pub voice: String,
    /// Empty while it runs; the reason it failed otherwise.
    pub error: Option<String>,
}

pub struct AppState {
    pub registry: BusinessRegistry,
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
    /// The ElevenLabs Agents platform, when the owner can hand calls to it.
    pub eleven_agents: Option<Arc<crate::eleven_agents::ElevenAgents>>,
    /// The conversations whose ride an ElevenLabs agent already sent (a retry sends nothing).
    pub(crate) tool_rides: Mutex<HashMap<String, std::time::Instant>>,
    /// The calls handed to an ElevenLabs agent that have not ended: Twilio call SID → start (unix).
    pub(crate) eleven_calls: Mutex<HashMap<String, i64>>,
    /// Conversations ElevenLabs confirmed as a live one of the chosen agent, for the length of a call.
    pub(crate) verified_conversations: Mutex<HashMap<String, std::time::Instant>>,
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
        let eleven_agents = settings
            .eleven_agents
            .clone()
            .map(|(key, base)| Arc::new(crate::eleven_agents::ElevenAgents::new(reqwest::Client::new(), key, base)));
        Arc::new(Self {
            registry,
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
            eleven_agents,
            tool_rides: Mutex::new(HashMap::new()),
            eleven_calls: Mutex::new(HashMap::new()),
            verified_conversations: Mutex::new(HashMap::new()),
        })
    }
}

impl AppState {
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
        let business = self.registry.by_id(business_id).ok_or_else(|| anyhow::anyhow!("unknown business"))?;
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
        for b in self.registry.all() {
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

/// The customer record with the caller's last ride from our own orders: its name, when the
/// record has none (a known caller is not asked for it again), and its pickup and destination
/// when they were found in the lists (`last_pickup`, `last_destination`, as said, and their
/// addresses). Those are only offered ("שוב מבן זכאי 45 באלעד?") and taken on a plain yes:
/// told the agent as facts, they once filled a garbled answer ("עזרא 11", heard "עשרה, אחד
/// עשרה", was booked as the last ride's "רבי אליעזר 11").
fn with_last_ride(
    customer: Option<Customer>,
    last: Option<(serde_json::Value, chrono::DateTime<chrono::Utc>)>,
) -> Option<Customer> {
    let customer = customer.map(|mut c| {
        c.name = c.name.filter(|n| plausible_name(n));
        c
    });
    let Some((card, _)) = last else { return customer };
    let mut places = serde_json::Map::new();
    for (field, key) in [("pickup", "last_pickup"), ("destination", "last_destination")] {
        let detail = card["details"].as_array().and_then(|d| d.iter().find(|d| d["field"] == field));
        let address = detail.and_then(|d| d["address"].as_str()).map(str::trim).unwrap_or("");
        let said = detail.and_then(|d| d["value"].as_str()).map(str::trim).unwrap_or("");
        // Only a place the lists knew: an unverified one ("לא מאומת") is not offered again.
        if address.is_empty() || said.is_empty() || address.contains("לא מאומת") {
            continue;
        }
        places.insert(key.into(), json!(spoken_place(said)));
        places.insert(format!("{key}_address"), json!(address));
    }
    let name = card["details"].as_array().and_then(|details| {
        details
            .iter()
            .find(|d| d["field"] == "customer_name")
            .and_then(|d| d["value"].as_str())
            .map(str::trim)
            .filter(|v| plausible_name(v))
            .map(str::to_string)
    });
    if name.is_none() && places.is_empty() {
        return customer;
    }
    let mut c = customer.unwrap_or_default();
    if c.name.is_none() {
        c.name = name;
    }
    for (k, v) in places {
        c.data.entry(k).or_insert(v);
    }
    Some(c)
}

/// "בן זכאי 45, אלעד" as it is said: "בן זכאי 45 באלעד".
fn spoken_place(said: &str) -> String {
    match said.rsplit_once(',') {
        Some((street, city)) if !street.trim().is_empty() && !city.trim().is_empty() => {
            format!("{} ב{}", street.trim(), city.trim())
        }
        _ => said.to_string(),
    }
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
        .route("/l/{token}", get(location_page).post(location_post))
        .route(twilio::WHISPER_PATH, post(whisper))
        .route(twilio::DESK_PATH, post(desk_answered))
        .route(twilio::DESK_STATUS_PATH, post(desk_status))
        .route(twilio::DESK_CONFERENCE_PATH, post(desk_conference))
        .route(&format!("{}/{{tool}}", crate::eleven_tools::TOOLS_PATH), post(eleven_tool))
        .route("/api/settings", get(api_settings))
        .route("/api/settings/{business}", axum::routing::put(api_save_settings))
        .route("/api/settings/{business}/price-bot", axum::routing::put(api_save_price_bot))
        .route("/api/settings/{business}/voice", axum::routing::put(api_save_voice))
        .route("/api/settings/agent-model", axum::routing::put(api_save_agent_model))
        .route("/api/settings/call-mode", axum::routing::put(api_save_call_mode))
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

/// The page a caller opens from the texted link (see `locations`): public, the token is the key.
async fn location_page(State(s): State<Arc<AppState>>, Path(token): Path<String>) -> Response {
    let live = s.services.locations.as_ref().is_some_and(|l| l.is_live(&token));
    if !live {
        return (StatusCode::NOT_FOUND, axum::response::Html("<p dir=\"rtl\">הקישור כבר לא בתוקף.</p>"))
            .into_response();
    }
    axum::response::Html(crate::locations::PAGE).into_response()
}

#[derive(Deserialize)]
struct Position {
    lat: f64,
    lon: f64,
}

async fn location_post(State(s): State<Arc<AppState>>, Path(token): Path<String>, Json(p): Json<Position>) -> Response {
    let delivered = s.services.locations.as_ref().is_some_and(|l| l.deliver(&token, p.lat, p.lon));
    if delivered {
        StatusCode::NO_CONTENT.into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
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
    let Some(business) = s.registry.by_number(&to) else {
        tracing::warn!(%to, "call to a number no business answers");
        return xml(twiml_unavailable());
    };
    if !s.settings.allow_list.is_empty() && !from.as_ref().is_some_and(|f| s.settings.allow_list.contains(f)) {
        tracing::info!(call = %call_sid, "caller not on the allow list");
        return xml(twilio::twiml_say_hangup("This number is not available.", "en-US"));
    }

    // The owner may have handed the phone to an ElevenLabs agent; if it does not answer,
    // Callora's own agent takes the call below.
    if let Some(twiml) = elevenlabs_twiml(&s, &call_sid, params.get("From").map_or("", String::as_str), &to).await {
        return xml(twiml);
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

/// The TwiML that connects the call to the ElevenLabs agent the owner chose, or none: not
/// chosen, not available, or ElevenLabs did not answer (then Callora's agent takes the call).
async fn elevenlabs_twiml(s: &AppState, call_sid: &str, from: &str, to: &str) -> Option<String> {
    let mode = s.services.settings.call_mode();
    if !mode.elevenlabs {
        return None;
    }
    let Some(agents) = &s.eleven_agents else {
        tracing::warn!(call = %call_sid, "ElevenLabs is chosen to answer but the server has no ElevenLabs key");
        return None;
    };
    match agents.register_call(mode.agent_id.trim(), from, to).await {
        Ok(twiml) => {
            tracing::info!(call = %call_sid, "the call goes to the ElevenLabs agent");
            // On the calls page from the first second, like any call; its transcript follows.
            let business_id = s.registry.by_number(to).map(|b| b.config.id.clone()).unwrap_or_default();
            s.services.store.record(CallRecord::Started {
                info: CallInfo {
                    call_id: crate::eleven_tools::eleven_call_id(call_sid),
                    call_sid: call_sid.to_string(),
                    business_id,
                    from: Some(from.to_string()).filter(|f| is_e164(f)),
                    to: to.to_string(),
                },
            });
            s.eleven_calls.lock().insert(call_sid.to_string(), chrono::Utc::now().timestamp());
            Some(twiml)
        }
        Err(e) => {
            tracing::warn!(call = %call_sid, error = %e, "ElevenLabs did not take the call; Callora's agent answers");
            None
        }
    }
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
            let started = s.eleven_calls.lock().remove(sid);
            if let Some(started) = started {
                // The call ends now, not when its transcript is read a few seconds later: the first
                // end is the one kept, and the transcript only adds its words and its outcome.
                s.services.store.record(CallRecord::Ended {
                    call_id: crate::eleven_tools::eleven_call_id(sid),
                    outcome: "completed".into(),
                    state: json!({ "source": "elevenlabs" }),
                    usage: Default::default(),
                });
                tokio::spawn(crate::eleven_tools::pull_transcript(s.clone(), sid.clone(), started));
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
    let Some(business) = s.registry.by_id(&claims.business_id) else { return };
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
    let businesses: Vec<_> = s.registry.all().map(|b| json!({ "id": b.config.id, "name": b.config.name })).collect();
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
        .registry
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
        .registry
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
        "call_mode": {
            "available": s.eleven_agents.is_some(),
            "tools_url": format!("{}{}", s.settings.public_base_url, crate::eleven_tools::TOOLS_PATH),
            "tools_token": crate::eleven_tools::tools_token(s.settings.stream_secrets.first().map_or("", String::as_str)),
            "elevenlabs": s.services.settings.call_mode().elevenlabs,
            "agent_id": s.services.settings.call_mode().agent_id,
        },
    }))
    .into_response()
}

/// The tools of an ElevenLabs agent (`create-ride`, `get-price`). Only with the token the settings
/// page shows the owner.
async fn eleven_tool(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(tool): Path<String>,
    body: Option<Json<serde_json::Value>>,
) -> Response {
    let secret = s.settings.stream_secrets.first().map_or("", String::as_str);
    let want = crate::eleven_tools::tools_token(secret);
    let given = headers.get("x-callora-tools-token").and_then(|v| v.to_str().ok()).unwrap_or("");
    let body = body.map_or(json!({}), |Json(b)| b);
    let by_token = !secret.is_empty() && bool::from(given.as_bytes().ct_eq(want.as_bytes()));
    if !by_token && !crate::eleven_tools::conversation_is_ours(&s, &body).await {
        tracing::warn!(
            %tool,
            token = if given.is_empty() { "missing" } else { "wrong" },
            "an ElevenLabs tool call was refused: no valid x-callora-tools-token, and ElevenLabs does not confirm its conversation_id as a live one of the chosen agent"
        );
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(crate::eleven_tools::run(&s, &tool, &body).await).into_response()
}

/// Choose who answers the phone: Callora's own agent, or an ElevenLabs agent (its id is checked
/// with ElevenLabs first). Saved, and used by the calls that come next.
async fn api_save_call_mode(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(mut mode): Json<crate::settings::CallModeSettings>,
) -> Response {
    if !authorized(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(pool) = &s.db else { return (StatusCode::SERVICE_UNAVAILABLE, "no database").into_response() };
    mode.agent_id = mode.agent_id.trim().to_string();
    let problems = |p: Vec<String>| (StatusCode::BAD_REQUEST, Json(json!({ "problems": p }))).into_response();
    if mode.elevenlabs {
        let Some(agents) = &s.eleven_agents else {
            return problems(vec!["אין בשרת מפתח של ElevenLabs".into()]);
        };
        if !mode.problems().is_empty() {
            return problems(mode.problems());
        }
        if let Err(problem) = agents.check_agent(&mode.agent_id).await {
            return problems(vec![problem]);
        }
    }
    if let Err(e) = s.services.settings.save_call_mode(pool, mode.clone()).await {
        tracing::error!(error = %e, "saving who answers the phone failed");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    tracing::info!(elevenlabs = mode.elevenlabs, "who answers the phone was changed");
    Json(json!({ "elevenlabs": mode.elevenlabs, "agent_id": mode.agent_id })).into_response()
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
async fn api_save_voice(
    State(s): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(business): Path<String>,
    Json(change): Json<VoiceChange>,
) -> Response {
    if !authorized(&s, &headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(b) = s.registry.by_id(&business) else { return StatusCode::NOT_FOUND.into_response() };
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
    if s.registry.by_id(&business).is_none() {
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
    let Some(b) = s.registry.by_id(&business) else { return StatusCode::NOT_FOUND.into_response() };
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
    if s.registry.by_id(&business).is_none() {
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
        assert!(c.places.is_empty(), "no saved places");
        // Its places, as said, to offer them; the unverified one is not.
        assert_eq!(c.data["last_pickup"], "בן זכאי 40 באלעד");
        assert_eq!(c.data["last_pickup_address"], "רבן יוחנן בן זכאי 40, אלעד");
        assert!(c.data.get("last_destination").is_none(), "no address: not offered");
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

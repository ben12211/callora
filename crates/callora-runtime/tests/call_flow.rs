//! A whole phone call against the real HTTP server: a fake Twilio client on the media
//! WebSocket, a scripted STT, the real taxi business, a voice library, and fake TTS and
//! telephony. Proves the greeting, booking, barge-in and hangup paths end to end.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use callora_audio::library::VoiceLibrary;
use callora_audio::tts::{AudioStream, Synthesizer, TtsCache, TtsRequest};
use callora_core::business::{Business, BusinessRegistry};
use callora_core::llm::LlmRequest;
use callora_core::render::library_entries;
use callora_runtime::actions::ConfiguredActions;
use callora_runtime::metrics::Metrics;
use callora_runtime::ports::{
    CallRecord, CallStore, LanguageModel, NoWhisper, SpeechToText, SttEvent, SttInput, SttSession, Telephony,
    TextStream,
};
use callora_runtime::server::{router, AppState, ServerSettings};
use callora_runtime::session::{Services, SessionConfig};
use callora_runtime::twilio;

const TAXI: &str = include_str!("../../../businesses/taxi.json");
const NUMBER: &str = "+972500000000";
const TOKEN: &str = "test-auth-token";

/// What the calls recorded, for the tests that look at it (each filters by its own call).
static RECORDS: Mutex<Vec<CallRecord>> = Mutex::new(Vec::new());

struct RecStore;

impl CallStore for RecStore {
    fn record(&self, record: CallRecord) {
        RECORDS.lock().push(record);
    }
}

/// STT whose transcripts the test pushes.
#[derive(Default, Clone)]
struct ScriptedStt {
    events: Arc<Mutex<Option<mpsc::Sender<SttEvent>>>>,
    finalizes: Arc<Mutex<usize>>,
    /// Every inbound audio byte the session handed to the recognizer.
    audio: Arc<Mutex<Vec<u8>>>,
    /// When set, the next end of speech is transcribed this late (and empty) instead of at once.
    slow_final: Arc<Mutex<Option<Duration>>>,
}

#[async_trait]
impl SpeechToText for ScriptedStt {
    async fn open(&self, _language: &str, _keyterms: &[String]) -> anyhow::Result<SttSession> {
        let (in_tx, mut in_rx) = mpsc::channel(1024);
        let (ev_tx, ev_rx) = mpsc::channel(16);
        *self.events.lock() = Some(ev_tx.clone());
        let finalizes = self.finalizes.clone();
        let audio = self.audio.clone();
        let slow = self.slow_final.clone();
        tokio::spawn(async move {
            while let Some(i) = in_rx.recv().await {
                match i {
                    SttInput::Finalize => {
                        *finalizes.lock() += 1;
                        // Like the real recognizers, every end of speech gets a transcript:
                        // empty here (the test pushes words with `say`), and late when slow.
                        let delay = slow.lock().take().unwrap_or(Duration::from_millis(50));
                        let tx = ev_tx.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(delay).await;
                            let _ = tx.send(SttEvent::Final(String::new())).await;
                        });
                    }
                    SttInput::Audio(a) => audio.lock().extend_from_slice(&a),
                    _ => {}
                }
            }
        });
        Ok(SttSession { input: in_tx, events: ev_rx })
    }
    fn name(&self) -> &'static str {
        "scripted"
    }
}

impl ScriptedStt {
    async fn say(&self, text: &str) {
        // The session opens STT in the background; wait for it rather than guess.
        let mut tx = None;
        for _ in 0..100 {
            tx = self.events.lock().clone();
            if tx.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let tx = tx.expect("stt opened within a second");
        tx.send(SttEvent::Final(text.into())).await.unwrap();
    }
}

/// TTS that returns one second of audio for anything.
struct FakeTts;

#[async_trait]
impl Synthesizer for FakeTts {
    async fn synthesize(&self, _req: TtsRequest) -> anyhow::Result<AudioStream> {
        tokio::time::sleep(Duration::from_millis(30)).await;
        let chunks: Vec<anyhow::Result<Bytes>> = (0..5).map(|_| Ok(Bytes::from(vec![0x33u8; 1600]))).collect();
        Ok(futures::stream::iter(chunks).boxed())
    }
    fn name(&self) -> &'static str {
        "fake"
    }
}

#[derive(Default)]
struct FakeTelephony {
    hangups: Mutex<Vec<String>>,
    /// Texts sent: (to, body).
    sms: Mutex<Vec<(String, String)>>,
}

#[async_trait]
impl Telephony for FakeTelephony {
    async fn hangup(&self, call_sid: &str) -> anyhow::Result<()> {
        self.hangups.lock().push(call_sid.to_string());
        Ok(())
    }
    async fn transfer(&self, _call_sid: &str, _to: &str, _whisper: Option<&str>) -> anyhow::Result<()> {
        Ok(())
    }
    fn sends_sms(&self) -> bool {
        true
    }
    async fn send_sms(&self, to: &str, body: &str) -> anyhow::Result<()> {
        self.sms.lock().push((to.to_string(), body.to_string()));
        Ok(())
    }
}

/// An agent whose replies the test scripts. Each reply streams in three chunks, 40 ms apart,
/// like a real model writing its JSON.
#[derive(Default)]
struct ScriptedAgent {
    replies: Mutex<std::collections::VecDeque<Value>>,
    requests: Mutex<Vec<LlmRequest>>,
}

#[async_trait]
impl LanguageModel for ScriptedAgent {
    async fn extract(&self, _request: &LlmRequest) -> anyhow::Result<Value> {
        anyhow::bail!("the agent streams")
    }

    async fn stream(&self, request: &LlmRequest) -> anyhow::Result<TextStream> {
        self.requests.lock().push(request.clone());
        let reply = self.replies.lock().pop_front().expect("a scripted reply").to_string();
        let n = reply.chars().count();
        let cut = |a: usize, b: usize| reply.chars().skip(a).take(b - a).collect::<String>();
        let chunks = vec![cut(0, n / 3), cut(n / 3, 2 * n / 3), cut(2 * n / 3, n)];
        Ok(futures::stream::iter(chunks)
            .then(|c| async move {
                tokio::time::sleep(Duration::from_millis(40)).await;
                Ok(c)
            })
            .boxed())
    }

    fn name(&self) -> &'static str {
        "scripted-agent"
    }
}

struct Harness {
    addr: std::net::SocketAddr,
    stt: ScriptedStt,
    telephony: Arc<FakeTelephony>,
    metrics: Arc<Metrics>,
}

async fn start_server() -> Harness {
    start_server_with(None).await
}

async fn start_server_with(agent: Option<Arc<dyn LanguageModel>>) -> Harness {
    // The tests tell clips apart by their bytes, so they play at 0 dB (bit-exact); the
    // gain has its own tests below.
    start_server_cfg(agent, SessionConfig { tts_gain_db: 0.0, ..SessionConfig::default() }).await
}

async fn start_server_cfg(agent: Option<Arc<dyn LanguageModel>>, session: SessionConfig) -> Harness {
    start_server_full(agent, session, None, Default::default()).await
}

/// The server with an ElevenLabs API (its base URL) and the owner's choice of who answers.
async fn start_server_full(
    agent: Option<Arc<dyn LanguageModel>>,
    session: SessionConfig,
    eleven: Option<String>,
    mode: callora_runtime::settings::CallModeSettings,
) -> Harness {
    start_server_inner(agent, session, eleven, mode, None).await
}

thread_local! {
    /// The next server in this test texts location links.
    static LINKS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The business the next server in this test runs, when not the taxi file as it is.
    static BUSINESS: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// The streets list and a second hearing, for the turns that answer a street or a city.
type Hearing = (Arc<callora_core::gazetteer::Gazetteer>, Arc<dyn callora_runtime::ports::Transcriber>);

async fn start_server_inner(
    agent: Option<Arc<dyn LanguageModel>>,
    session: SessionConfig,
    eleven: Option<String>,
    mode: callora_runtime::settings::CallModeSettings,
    hearing: Option<Hearing>,
) -> Harness {
    let env = |k: &str| {
        (k == "TAXI_PHONE_NUMBERS")
            .then(|| NUMBER.to_string())
            .or_else(|| (k == "ELEVENLABS_VOICE_ID").then(|| "voice".into()))
    };
    let json = BUSINESS.with(|b| b.borrow_mut().take()).unwrap_or_else(|| TAXI.to_string());
    let business = Business::from_json(&json, "taxi.json", &env).unwrap();
    // Every pre-generable sentence is in the library: 0x55 audio, 3 frames each.
    let mut library = VoiceLibrary::empty();
    for e in library_entries(&business) {
        library.insert(&e.delivery, &e.text, Bytes::from(vec![0x55u8; 480]));
    }
    let registry = BusinessRegistry::new(vec![business]).unwrap();
    let stt = ScriptedStt::default();
    let telephony = Arc::new(FakeTelephony::default());
    let metrics = Arc::new(Metrics::default());
    let services = Services {
        stt: Arc::new(stt.clone()),
        llm: None,
        agent,
        gazetteer: hearing.as_ref().map(|h| h.0.clone()),
        second_hearing: hearing.map(|h| h.1),
        tts: Some(Arc::new(FakeTts)),
        tts_cache: TtsCache::new(100),
        actions: Arc::new(ConfiguredActions::new(reqwest::Client::new(), HashMap::new())),
        telephony: telephony.clone(),
        store: Arc::new(RecStore),
        whisper: Arc::new(NoWhisper),
        metrics: metrics.clone(),
        settings: {
            let store = callora_runtime::settings::SettingsStore::default();
            store.set_call_mode(mode);
            Arc::new(store)
        },
        desk: None,
        locations: LINKS
            .with(|l| l.replace(false))
            .then(|| Arc::new(callora_runtime::locations::LocationLinks::new("https://calls.example.test"))),
    };
    let mut libraries = HashMap::new();
    libraries.insert("taxi".to_string(), Arc::new(library));
    let settings = ServerSettings {
        public_base_url: "https://calls.example.test".into(),
        twilio_auth_token: TOKEN.into(),
        stream_secrets: vec![TOKEN.into()],
        allow_list: vec![],
        admin_api_key: None,
        skip_signature_validation: false,
        prices: Default::default(),
        dashboard_password: "12345678".into(),
        web_dir: None,
        whatsapp: None,
        library_dir: None,
        library_model: None,
        eleven_agents: eleven.map(|base| ("xi-test-key".to_string(), Some(base))),
    };
    let state = AppState::new(registry, libraries, services, session, settings, None);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    Harness { addr, stt, telephony, metrics }
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Media frames (first payload byte of each) and clears received within `window`.
async fn collect(ws: &mut Ws, window: Duration) -> (Vec<u8>, usize) {
    let mut frames = Vec::new();
    let mut clears = 0;
    let deadline = tokio::time::Instant::now() + window;
    while let Ok(Some(Ok(msg))) = tokio::time::timeout_at(deadline, ws.next()).await {
        let Message::Text(text) = msg else { continue };
        let v: Value = serde_json::from_str(&text).unwrap();
        match v["event"].as_str() {
            Some("media") => {
                let audio =
                    base64::engine::general_purpose::STANDARD.decode(v["media"]["payload"].as_str().unwrap()).unwrap();
                assert_eq!(audio.len(), 160, "20 ms frames");
                frames.push(audio[0]);
            }
            Some("clear") => clears += 1,
            _ => {}
        }
    }
    (frames, clears)
}

fn loud_frame() -> String {
    let audio: Vec<u8> = (0..160).map(|i| if i % 2 == 0 { 0x10 } else { 0x90 }).collect();
    json!({ "event": "media", "streamSid": "MZ1", "media": { "track": "inbound", "payload": base64::engine::general_purpose::STANDARD.encode(audio) } }).to_string()
}

fn quiet_frame() -> String {
    json!({ "event": "media", "streamSid": "MZ1", "media": { "track": "inbound", "payload": base64::engine::general_purpose::STANDARD.encode([0xFFu8; 160]) } }).to_string()
}

#[tokio::test]
async fn voice_webhook_requires_a_valid_signature_and_returns_a_stream() {
    let h = start_server().await;
    let client = reqwest::Client::new();
    let mut params = BTreeMap::new();
    params.insert("CallSid".to_string(), "CA1".to_string());
    params.insert("To".to_string(), NUMBER.to_string());
    params.insert("From".to_string(), "+972501111111".to_string());
    let url = format!("http://{}{}", h.addr, twilio::VOICE_PATH);

    let bad = client.post(&url).header("X-Twilio-Signature", "bogus").form(&params).send().await.unwrap();
    assert_eq!(bad.status(), 403);

    let sig = twilio::signature(TOKEN, &format!("https://calls.example.test{}", twilio::VOICE_PATH), &params);
    let ok = client.post(&url).header("X-Twilio-Signature", sig).form(&params).send().await.unwrap();
    assert_eq!(ok.status(), 200);
    let body = ok.text().await.unwrap();
    assert!(body.contains("<Connect><Stream url=\"wss://calls.example.test/webhooks/twilio/media\">"), "{body}");
    assert!(body.contains("<Parameter name=\"token\""));
}

/// A stand-in for ElevenLabs' register-call: answers with `reply` and remembers what it was asked.
async fn fake_elevenlabs(reply: (u16, String)) -> (String, Arc<Mutex<Vec<(Value, Option<String>)>>>) {
    use axum::http::{HeaderMap, StatusCode};
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let app = axum::Router::new()
        .route(
            "/v1/convai/conversations/{id}",
            axum::routing::get(|axum::extract::Path(id): axum::extract::Path<String>| async move {
                match id.as_str() {
                    "conv-live" => (
                        StatusCode::OK,
                        axum::Json(json!({ "agent_id": "agent_abc123", "status": "in-progress",
                            "metadata": { "phone_call": { "call_sid": "CA-eleven" } } })),
                    ),
                    "conv-done" => {
                        (StatusCode::OK, axum::Json(json!({ "agent_id": "agent_abc123", "status": "done" })))
                    }
                    "conv-other" => (
                        StatusCode::OK,
                        axum::Json(json!({ "agent_id": "agent_someone_else", "status": "in-progress" })),
                    ),
                    _ => (StatusCode::NOT_FOUND, axum::Json(json!({}))),
                }
            }),
        )
        .route(
            "/v1/convai/twilio/register-call",
            axum::routing::post(move |headers: HeaderMap, axum::Json(body): axum::Json<Value>| {
                let log = log.clone();
                let reply = reply.clone();
                async move {
                    let key = headers.get("xi-api-key").and_then(|v| v.to_str().ok()).map(str::to_string);
                    log.lock().push((body, key));
                    (StatusCode::from_u16(reply.0).unwrap(), reply.1)
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

async fn voice_call(h: &Harness) -> String {
    let mut params = BTreeMap::new();
    params.insert("CallSid".to_string(), "CA-eleven".to_string());
    params.insert("To".to_string(), NUMBER.to_string());
    params.insert("From".to_string(), "+972501111111".to_string());
    let sig = twilio::signature(TOKEN, &format!("https://calls.example.test{}", twilio::VOICE_PATH), &params);
    let url = format!("http://{}{}", h.addr, twilio::VOICE_PATH);
    let ok = reqwest::Client::new().post(&url).header("X-Twilio-Signature", sig).form(&params).send().await.unwrap();
    assert_eq!(ok.status(), 200);
    ok.text().await.unwrap()
}

#[tokio::test]
async fn a_call_goes_to_the_elevenlabs_agent_when_the_owner_chose_it() {
    let twiml =
        "<?xml version=\"1.0\"?><Response><Connect><Stream url=\"wss://elevenlabs.test/s\"/></Connect></Response>";
    let (base, seen) = fake_elevenlabs((200, twiml.to_string())).await;
    let mode = callora_runtime::settings::CallModeSettings { elevenlabs: true, agent_id: "agent_abc123".into() };
    let h = start_server_full(None, SessionConfig::default(), Some(base), mode).await;
    let body = voice_call(&h).await;
    assert_eq!(body, twiml, "ElevenLabs' TwiML is what Twilio gets");
    let seen = seen.lock();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0["agent_id"], "agent_abc123");
    assert_eq!(seen[0].0["from_number"], "+972501111111");
    assert_eq!(seen[0].0["to_number"], NUMBER);
    assert_eq!(seen[0].1.as_deref(), Some("xi-test-key"));
}

#[tokio::test]
async fn when_elevenlabs_does_not_take_the_call_callora_answers() {
    let (base, seen) = fake_elevenlabs((500, "down".to_string())).await;
    let mode = callora_runtime::settings::CallModeSettings { elevenlabs: true, agent_id: "agent_abc123".into() };
    let h = start_server_full(None, SessionConfig::default(), Some(base), mode).await;
    let body = voice_call(&h).await;
    assert!(body.contains("<Connect><Stream url=\"wss://calls.example.test/webhooks/twilio/media\">"), "{body}");
    assert_eq!(seen.lock().len(), 1, "it was tried first");
}

async fn tool(h: &Harness, name: &str, token: Option<&str>, body: Value) -> (u16, Value) {
    let mut req =
        reqwest::Client::new().post(format!("http://{}/webhooks/elevenlabs/tools/{name}", h.addr)).json(&body);
    if let Some(t) = token {
        req = req.header("x-callora-tools-token", t);
    }
    let res = req.send().await.unwrap();
    let status = res.status().as_u16();
    (status, res.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
async fn the_elevenlabs_tools_need_the_token_and_send_a_ride_once() {
    let h = start_server().await;
    let good = callora_runtime::eleven_tools::tools_token(TOKEN);
    let ride = json!({
        "pickup": "כניסה לעיר, ביתר עילית",
        "destination": "סינמה סיטי, ירושלים",
        "passengers": 3,
        "customer_name": "יוסי כהן",
        "notes": "",
        "conversation_id": "conv-1",
        "caller_number": "+972501111111",
    });

    assert_eq!(tool(&h, "create-ride", None, ride.clone()).await.0, 401, "no token");
    assert_eq!(tool(&h, "create-ride", Some("wrong"), ride.clone()).await.0, 401, "wrong token");

    let (status, sent) = tool(&h, "create-ride", Some(&good), ride.clone()).await;
    assert_eq!((status, &sent["ok"]), (200, &json!(true)), "{sent}");
    assert_eq!(sent["message"], "הנסיעה נשלחה");

    // The same conversation telling the same ride again (a retried tool call) sends nothing.
    let (_, again) = tool(&h, "create-ride", Some(&good), ride.clone()).await;
    assert_eq!(again["duplicate"], true, "{again}");

    // A ride with no passengers is not sent, and the agent gets a sentence to speak from.
    let mut bad = ride;
    bad["passengers"] = json!(0);
    bad["conversation_id"] = json!("conv-2");
    let (_, refused) = tool(&h, "create-ride", Some(&good), bad).await;
    assert_eq!(refused["ok"], false);
    assert!(refused["message"].as_str().is_some_and(|m| !m.is_empty()), "{refused}");

    // No price list is configured in the test: an answer the agent can say, not an error.
    let (status, price) =
        tool(&h, "get-price", Some(&good), json!({ "price_from": "בני ברק", "price_to": "ירושלים" })).await;
    assert_eq!(status, 200);
    assert_eq!(price["ok"], false);
    assert!(price["message"].as_str().is_some_and(|m| !m.is_empty()), "{price}");
}

#[tokio::test]
async fn an_elevenlabs_call_is_on_the_calls_page_and_its_ride_joins_it() {
    let (base, _) = fake_elevenlabs((200, "<Response/>".to_string())).await;
    let mode = callora_runtime::settings::CallModeSettings { elevenlabs: true, agent_id: "agent_abc123".into() };
    let h = start_server_full(None, SessionConfig::default(), Some(base), mode).await;
    voice_call(&h).await; // CallSid CA-eleven
    let id = callora_runtime::eleven_tools::eleven_call_id("CA-eleven");
    let mine = |r: &CallRecord| match r {
        CallRecord::Started { info } => info.call_id == id,
        CallRecord::Order { call_id, .. } | CallRecord::Ended { call_id, .. } => *call_id == id,
        _ => false,
    };
    assert!(
        RECORDS.lock().iter().any(|r| matches!(r, CallRecord::Started { info } if info.call_id == id && info.call_sid == "CA-eleven" && info.from.as_deref() == Some("+972501111111"))),
        "the call is on the calls page from the moment it is handed over"
    );

    let token = callora_runtime::eleven_tools::tools_token(TOKEN);
    let ride = json!({
        "pickup": "הנביאים, ירושלים", "destination": "רוטשילד, תל אביב", "passengers": 2,
        "customer_name": "דיאן כהן", "notes": "מזוודות", "call_sid": "CA-eleven", "conversation_id": "conv-phone",
    });
    let (_, sent) = tool(&h, "create-ride", Some(&token), ride).await;
    assert_eq!(sent["ok"], true, "{sent}");
    let records = RECORDS.lock();
    let orders = records.iter().filter(|r| matches!(r, CallRecord::Order { call_id, .. } if *call_id == id)).count();
    assert_eq!(orders, 1, "the ride is an order of that call");
    assert!(
        !records.iter().any(|r| mine(r) && matches!(r, CallRecord::Ended { .. })),
        "a phone call ends with its transcript, not with the ride"
    );
}

#[tokio::test]
async fn a_transfer_with_no_desk_says_so_instead_of_promising_one() {
    let h = start_server().await;
    let token = callora_runtime::eleven_tools::tools_token(TOKEN);
    let (_, none) = tool(&h, "transfer-to-desk", Some(&token), json!({ "call_sid": "CA-x", "summary": "x" })).await;
    assert_eq!(none["ok"], false, "{none}");
    let (_, no_call) = tool(&h, "transfer-to-desk", Some(&token), json!({ "summary": "x" })).await;
    assert_eq!(no_call["ok"], false, "{no_call}");
}

#[tokio::test]
async fn a_tool_call_needs_no_token_when_elevenlabs_confirms_its_live_conversation() {
    let (base, _) = fake_elevenlabs((200, "<Response/>".to_string())).await;
    let mode = callora_runtime::settings::CallModeSettings { elevenlabs: true, agent_id: "agent_abc123".into() };
    let h = start_server_full(None, SessionConfig::default(), Some(base), mode).await;
    let ride = |conversation: &str, sid: &str| {
        json!({ "pickup": "הנביאים, ירושלים", "destination": "רוטשילד, תל אביב", "passengers": 2,
                "customer_name": "דיאן", "conversation_id": conversation, "call_sid": sid })
    };

    let (status, sent) = tool(&h, "create-ride", None, ride("conv-live", "")).await;
    assert_eq!((status, &sent["ok"]), (200, &json!(true)), "a live conversation of our agent: {sent}");

    assert_eq!(tool(&h, "create-ride", None, ride("conv-done", "")).await.0, 401, "a finished one");
    assert_eq!(tool(&h, "create-ride", None, ride("conv-other", "")).await.0, 401, "another agent's");
    assert_eq!(tool(&h, "create-ride", None, ride("conv-unknown", "")).await.0, 401, "one that does not exist");
    assert_eq!(
        tool(&h, "create-ride", None, json!({ "pickup": "x", "destination": "y", "passengers": 1 })).await.0,
        401
    );
}

#[tokio::test]
async fn callora_answers_unless_the_owner_chose_elevenlabs() {
    let (base, seen) = fake_elevenlabs((200, "<Response/>".to_string())).await;
    let h = start_server_full(None, SessionConfig::default(), Some(base), Default::default()).await;
    let body = voice_call(&h).await;
    assert!(body.contains("<Connect><Stream url=\"wss://calls.example.test/webhooks/twilio/media\">"), "{body}");
    assert!(seen.lock().is_empty(), "ElevenLabs is never asked");
}

#[tokio::test]
async fn desk_callbacks_require_signatures_over_the_token_and_form() {
    let h = start_server().await;
    let http = reqwest::Client::new();
    for path in [twilio::DESK_PATH, twilio::DESK_STATUS_PATH, twilio::DESK_CONFERENCE_PATH, twilio::STATUS_PATH] {
        let path = format!("{path}?t=test-token");
        let public = format!("https://calls.example.test{path}");
        let local = format!("http://{}{path}", h.addr);
        let params = BTreeMap::from([
            ("CallSid".to_string(), "CA-test".to_string()),
            ("CallStatus".to_string(), "busy".to_string()),
        ]);
        assert_eq!(http.post(&local).form(&params).send().await.unwrap().status(), 403);
        let signature = twilio::signature(TOKEN, &public, &params);
        assert!(http
            .post(&local)
            .form(&params)
            .header("x-twilio-signature", &signature)
            .send()
            .await
            .unwrap()
            .status()
            .is_success());
        let wrong_token = local.replace("test-token", "other-token");
        assert_eq!(
            http.post(wrong_token)
                .form(&params)
                .header("x-twilio-signature", &signature)
                .send()
                .await
                .unwrap()
                .status(),
            403
        );
    }
}

#[tokio::test]
async fn a_full_call_greeting_booking_barge_in_and_goodbye() {
    let h = start_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}{}", h.addr, twilio::MEDIA_PATH)).await.unwrap();
    let token = twilio::create_stream_token(TOKEN, "CA42", "taxi", 300, chrono_now());
    ws.send(Message::Text(json!({ "event": "connected" }).to_string().into())).await.unwrap();
    ws.send(Message::Text(json!({ "event": "start", "streamSid": "MZ1", "start": { "streamSid": "MZ1", "callSid": "CA42", "customParameters": { "token": token } } }).to_string().into())).await.unwrap();

    // Greeting: pre-generated, so it starts at once.
    let started = tokio::time::Instant::now();
    let first =
        tokio::time::timeout(Duration::from_millis(500), ws.next()).await.expect("greeting arrives").unwrap().unwrap();
    assert!(started.elapsed() < Duration::from_millis(400), "greeting took {:?}", started.elapsed());
    assert!(first.to_text().unwrap().contains("\"media\""));
    let (frames, _) = collect(&mut ws, Duration::from_millis(300)).await;
    assert!(frames.iter().all(|b| *b == 0x55), "greeting comes from the library");

    // The caller asks for everything in one sentence.
    h.stt.say("צריך מונית עכשיו מרבי עקיבא 12 לנתב\"ג, אנחנו ארבעה").await;
    // "סגור." from the library, then the read-back (dynamic: it contains an address).
    let (frames, _) = collect(&mut ws, Duration::from_millis(250)).await;
    assert!(
        frames.first() == Some(&0x55),
        "the cached acknowledgement plays first: {:?}",
        &frames[..frames.len().min(5)]
    );
    assert!(frames.contains(&0x33), "then the dynamic read-back");

    // A short sound over the read-back (a cough) does not stop it.
    for _ in 0..8 {
        ws.send(Message::Text(loud_frame().into())).await.unwrap();
    }
    let (_, clears) = collect(&mut ws, Duration::from_millis(100)).await;
    assert_eq!(clears, 0, "a cough is not a barge-in");
    // The caller talks over the read-back: audio stops and Twilio is told to clear.
    for _ in 0..70 {
        ws.send(Message::Text(loud_frame().into())).await.unwrap();
    }
    let (_, clears) = collect(&mut ws, Duration::from_millis(150)).await;
    assert!(clears >= 1, "barge-in clears Twilio's buffer");
    for _ in 0..40 {
        ws.send(Message::Text(quiet_frame().into())).await.unwrap();
    }
    let (frames, _) = collect(&mut ws, Duration::from_millis(200)).await;
    assert!(frames.is_empty(), "nothing plays after the barge-in");
    assert!(*h.stt.finalizes.lock() >= 1, "the VAD endpoint asked the STT to finalize");

    // "כן" to a read-back cut off in the middle: the read-back again, not the booking.
    h.stt.say("כן").await;
    let (frames, _) = collect(&mut ws, Duration::from_millis(400)).await;
    assert!(!frames.is_empty(), "the read-back again");
    while !collect(&mut ws, Duration::from_millis(300)).await.0.is_empty() {}

    // "כן" → filler immediately, then the booking result (mock backend, ~0.9 s).
    h.stt.say("כן").await;
    let (frames, _) = collect(&mut ws, Duration::from_millis(1600)).await;
    assert!(frames.len() > 6, "filler and booking confirmation played: {}", frames.len());

    // Done: goodbye, then the agent hangs up once the goodbye has played.
    h.stt.say("לא, תודה").await;
    let (frames, _) = collect(&mut ws, Duration::from_millis(1200)).await;
    assert!(!frames.is_empty(), "goodbye played");
    assert_eq!(h.telephony.hangups.lock().as_slice(), ["CA42"]);

    let metrics = h.metrics.render();
    assert!(metrics.contains("callora_barge_ins_total 1"), "{metrics}");
    assert!(h.metrics.response_latency.count() >= 2);
}

#[tokio::test]
async fn a_stream_with_a_forged_token_is_refused() {
    let h = start_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}{}", h.addr, twilio::MEDIA_PATH)).await.unwrap();
    let token = twilio::create_stream_token("wrong-secret", "CA9", "taxi", 300, chrono_now());
    ws.send(Message::Text(json!({ "event": "start", "streamSid": "MZ9", "start": { "streamSid": "MZ9", "callSid": "CA9", "customParameters": { "token": token } } }).to_string().into())).await.unwrap();
    let (frames, _) = collect(&mut ws, Duration::from_millis(300)).await;
    assert!(frames.is_empty(), "no audio for an unauthorized stream");
}

#[tokio::test]
async fn the_agent_runs_the_call_and_its_first_sentence_plays_while_it_is_still_writing() {
    let agent = Arc::new(ScriptedAgent::default());
    agent.replies.lock().extend([
        json!({ "say": "לאן נוסעים?", "action": "none", "task": "book_ride",
                "fields": [{ "slot": "pickup", "value": "באר שבע" }] }),
        json!({ "say": "סגור.", "action": "read_back", "task": "book_ride",
                "fields": [{ "slot": "destination", "value": "תל אביב" }, { "slot": "passengers", "value": "שניים" },
                           { "slot": "notes", "value": "יש מזוודה" }] }),
    ]);
    let h = start_server_with(Some(agent.clone())).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}{}", h.addr, twilio::MEDIA_PATH)).await.unwrap();
    let token = twilio::create_stream_token(TOKEN, "CA44", "taxi", 300, chrono_now());
    ws.send(Message::Text(json!({ "event": "connected" }).to_string().into())).await.unwrap();
    ws.send(Message::Text(json!({ "event": "start", "streamSid": "MZ1", "start": { "streamSid": "MZ1", "callSid": "CA44", "customParameters": { "token": token } } }).to_string().into())).await.unwrap();
    collect(&mut ws, Duration::from_millis(400)).await;

    // "לאן נוסעים?" is an instant phrase: it plays from the library as soon as its sentence
    // is complete in the stream, before the model has written its decision.
    h.stt.say("אני רוצה מונית מבאר שבע").await;
    let started = tokio::time::Instant::now();
    let first = tokio::time::timeout(Duration::from_millis(500), ws.next()).await.expect("audio").unwrap().unwrap();
    assert!(started.elapsed() < Duration::from_millis(200), "first words after {:?}", started.elapsed());
    assert!(first.to_text().unwrap().contains("\"media\""));
    let (frames, _) = collect(&mut ws, Duration::from_millis(300)).await;
    assert!(frames.iter().all(|b| *b == 0x55), "a recorded clip, no live TTS");

    // The agent asks for the read-back: "סגור." (recorded), then the engine's read-back.
    h.stt.say("לתל אביב, אנחנו שניים").await;
    let (frames, _) = collect(&mut ws, Duration::from_millis(800)).await;
    assert_eq!(frames.first(), Some(&0x55), "{:?}", &frames[..frames.len().min(5)]);
    assert!(frames.contains(&0x33), "the read-back has live parts (the addresses)");

    // A plain yes to the read-back skips the agent: the booking starts at once.
    h.stt.say("כן").await;
    collect(&mut ws, Duration::from_millis(300)).await;
    let requests = agent.requests.lock();
    assert_eq!(requests.len(), 2, "the yes went through the fast lane");
    assert!(requests[1].user.contains("Agent: לאן נוסעים?"), "the agent sees the conversation: {}", requests[1].user);
    assert!(requests[1].user.contains("- pickup: באר שבע"), "and the booking so far: {}", requests[1].user);
}

#[tokio::test]
async fn a_recorded_phrase_split_into_sentences_still_plays_as_its_clip() {
    // "הכל טוב, תודה! איך אפשר לעזור?" streams as two sentences; the first alone is not a
    // recording. It must wait for the second, not go through live TTS (a live call did).
    let agent = Arc::new(ScriptedAgent::default());
    agent.replies.lock().push_back(
        json!({ "action": "none", "say": "הכל טוב, תודה! איך אפשר לעזור?", "task": "small_talk", "fields": [] }),
    );
    let h = start_server_with(Some(agent)).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}{}", h.addr, twilio::MEDIA_PATH)).await.unwrap();
    let token = twilio::create_stream_token(TOKEN, "CA45", "taxi", 300, chrono_now());
    ws.send(Message::Text(json!({ "event": "connected" }).to_string().into())).await.unwrap();
    ws.send(Message::Text(json!({ "event": "start", "streamSid": "MZ1", "start": { "streamSid": "MZ1", "callSid": "CA45", "customParameters": { "token": token } } }).to_string().into())).await.unwrap();
    collect(&mut ws, Duration::from_millis(400)).await;

    h.stt.say("מה המצב?").await;
    let (frames, _) = collect(&mut ws, Duration::from_millis(500)).await;
    assert!(!frames.is_empty(), "the answer plays");
    assert!(frames.iter().all(|b| *b == 0x55), "one recorded clip, no live TTS: {:?}", &frames[..frames.len().min(8)]);
}

#[tokio::test]
async fn two_recorded_phrases_in_one_reply_play_as_two_clips() {
    // "הכל טוב, תודה!" waits (it begins a longer phrase), but "מאיפה יוצאים?" does not
    // continue it: each plays as its own recording, not the pair as one live TTS.
    let agent = Arc::new(ScriptedAgent::default());
    agent.replies.lock().push_back(
        json!({ "action": "none", "say": "הכל טוב, תודה! מאיפה יוצאים?", "task": "book_ride", "fields": [] }),
    );
    let h = start_server_with(Some(agent)).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}{}", h.addr, twilio::MEDIA_PATH)).await.unwrap();
    let token = twilio::create_stream_token(TOKEN, "CA46", "taxi", 300, chrono_now());
    ws.send(Message::Text(json!({ "event": "connected" }).to_string().into())).await.unwrap();
    ws.send(Message::Text(json!({ "event": "start", "streamSid": "MZ1", "start": { "streamSid": "MZ1", "callSid": "CA46", "customParameters": { "token": token } } }).to_string().into())).await.unwrap();
    collect(&mut ws, Duration::from_millis(400)).await;

    h.stt.say("מה מצב? אני רוצה להזמין מונית").await;
    let (frames, _) = collect(&mut ws, Duration::from_millis(500)).await;
    assert!(frames.len() >= 6, "both phrases play: {} frames", frames.len());
    assert!(frames.iter().all(|b| *b == 0x55), "recordings only: {:?}", &frames[..frames.len().min(8)]);
}

#[tokio::test]
async fn a_phrase_id_plays_its_recording_once_and_the_agent_hears_what_was_said() {
    // The model names a recorded phrase instead of writing its words; writing them again in
    // `say` as well must not say them twice.
    let agent = Arc::new(ScriptedAgent::default());
    agent.replies.lock().extend([
        json!({ "action": "none", "fields": [{ "slot": "pickup", "value": "באר שבע" }],
                "phrase": "ask_destination", "say": "לאן נוסעים?", "task": "book_ride" }),
        json!({ "action": "none", "fields": [], "phrase": null, "say": "", "task": "book_ride" }),
    ]);
    let h = start_server_with(Some(agent.clone())).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}{}", h.addr, twilio::MEDIA_PATH)).await.unwrap();
    let token = twilio::create_stream_token(TOKEN, "CA47", "taxi", 300, chrono_now());
    ws.send(Message::Text(json!({ "event": "connected" }).to_string().into())).await.unwrap();
    ws.send(Message::Text(json!({ "event": "start", "streamSid": "MZ1", "start": { "streamSid": "MZ1", "callSid": "CA47", "customParameters": { "token": token } } }).to_string().into())).await.unwrap();
    collect(&mut ws, Duration::from_millis(400)).await;

    h.stt.say("מונית מבאר שבע").await;
    let (frames, _) = collect(&mut ws, Duration::from_millis(500)).await;
    assert_eq!(frames.len(), 3, "one recorded clip (3 frames), said once: {frames:?}");
    assert!(frames.iter().all(|b| *b == 0x55), "recorded, no live TTS");

    h.stt.say("לתל אביב").await;
    collect(&mut ws, Duration::from_millis(300)).await;
    let requests = agent.requests.lock();
    let asked = ["לאן נוסעים?", "ולאן?", "לאן צריך להגיע?"];
    let said: Vec<&str> = requests[1].user.lines().filter(|l| l.starts_with("Agent: ")).collect();
    assert!(said.iter().any(|l| asked.iter().any(|a| l.ends_with(a))), "the agent sees its phrase: {said:?}");
    assert!(requests[1].user.contains("- pickup: באר שבע"), "{}", requests[1].user);
}

fn chrono_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

#[tokio::test]
async fn a_reply_that_starts_with_live_tts_plays_it() {
    let h = start_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}{}", h.addr, twilio::MEDIA_PATH)).await.unwrap();
    let token = twilio::create_stream_token(TOKEN, "CA43", "taxi", 300, chrono_now());
    ws.send(Message::Text(json!({ "event": "connected" }).to_string().into())).await.unwrap();
    ws.send(Message::Text(json!({ "event": "start", "streamSid": "MZ1", "start": { "streamSid": "MZ1", "callSid": "CA43", "customParameters": { "token": token } } }).to_string().into())).await.unwrap();
    collect(&mut ws, Duration::from_millis(400)).await;

    h.stt.say("צריך מונית").await;
    collect(&mut ws, Duration::from_millis(400)).await;
    // A one-word pickup is doubtful, so it is read back: "<value>, נכון?" needs live TTS.
    h.stt.say("מזרחי").await;
    let (frames, _) = collect(&mut ws, Duration::from_millis(400)).await;
    // No recorded opener before it any more ("אהה, אוקיי." was dropped from the business).
    assert!(frames.contains(&0x33), "the live read-back plays: {:?}", &frames[..frames.len().min(5)]);
}

#[tokio::test]
async fn words_taken_for_noise_before_any_task_do_not_repeat_the_greeting() {
    // The call of 23:58 heard its greeting twice: noise right after it asked "again".
    // Before a task there is no question to repeat; the silence reprompt waits its 5 s.
    let h = start_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}{}", h.addr, twilio::MEDIA_PATH)).await.unwrap();
    let token = twilio::create_stream_token(TOKEN, "CA43", "taxi", 300, chrono_now());
    ws.send(Message::Text(json!({ "event": "connected" }).to_string().into())).await.unwrap();
    ws.send(Message::Text(json!({ "event": "start", "streamSid": "MZ1", "start": { "streamSid": "MZ1", "callSid": "CA43", "customParameters": { "token": token } } }).to_string().into())).await.unwrap();
    let (frames, _) = collect(&mut ws, Duration::from_millis(400)).await;
    assert!(!frames.is_empty(), "greeting");

    h.stt.say("אה").await;
    let (frames, _) = collect(&mut ws, Duration::from_millis(3000)).await;
    assert!(frames.is_empty(), "the greeting is not said again");
}

#[tokio::test]
async fn a_sound_with_no_words_gets_the_question_again_instead_of_silence() {
    // Noise the recognizer returned nothing for: the silence reprompt had been cancelled by the
    // sound, and nothing more was said until the caller spoke.
    let h = start_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}{}", h.addr, twilio::MEDIA_PATH)).await.unwrap();
    let token = twilio::create_stream_token(TOKEN, "CA49", "taxi", 300, chrono_now());
    ws.send(Message::Text(json!({ "event": "connected" }).to_string().into())).await.unwrap();
    ws.send(Message::Text(json!({ "event": "start", "streamSid": "MZ1", "start": { "streamSid": "MZ1", "callSid": "CA49", "customParameters": { "token": token } } }).to_string().into())).await.unwrap();
    collect(&mut ws, Duration::from_millis(400)).await;
    h.stt.say("צריך מונית").await;
    while !collect(&mut ws, Duration::from_millis(300)).await.0.is_empty() {}

    for _ in 0..20 {
        ws.send(Message::Text(loud_frame().into())).await.unwrap();
    }
    for _ in 0..40 {
        ws.send(Message::Text(quiet_frame().into())).await.unwrap();
    }
    let (frames, _) = collect(&mut ws, Duration::from_millis(1500)).await;
    assert!(frames.is_empty(), "not at once: the words may still come");
    let (frames, _) = collect(&mut ws, Duration::from_millis(1500)).await;
    assert!(!frames.is_empty(), "the line is noisy, and the question again");
}

async fn noise_call() -> (Harness, Ws) {
    let h = start_server().await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}{}", h.addr, twilio::MEDIA_PATH)).await.unwrap();
    let token = twilio::create_stream_token(TOKEN, "CA-no-words", "taxi", 300, chrono_now());
    ws.send(Message::Text(json!({ "event": "start", "start": { "streamSid": "MZ1", "callSid": "CA-no-words", "customParameters": { "token": token } } }).to_string().into())).await.unwrap();
    collect(&mut ws, Duration::from_millis(400)).await;
    h.stt.say("צריך מונית").await;
    while !collect(&mut ws, Duration::from_millis(300)).await.0.is_empty() {}
    for _ in 0..20 {
        ws.send(Message::Text(loud_frame().into())).await.unwrap();
    }
    for _ in 0..40 {
        ws.send(Message::Text(quiet_frame().into())).await.unwrap();
    }
    (h, ws)
}

#[tokio::test]
async fn a_slow_transcript_is_waited_for_not_called_noise() {
    // The call of 13:36: "בן זכאי ארבעים וחמש" took 2.3 s to transcribe, and at 2 s the agent
    // said "סליחה, יש קצת רעש בקו" and asked the question it had just been answered.
    let h = start_server().await;
    let mut ws = open_call(&h, "CA-slow-final").await;
    collect(&mut ws, Duration::from_millis(400)).await;
    h.stt.say("צריך מונית").await;
    while !collect(&mut ws, Duration::from_millis(300)).await.0.is_empty() {}
    *h.stt.slow_final.lock() = Some(Duration::from_secs(4));
    for _ in 0..20 {
        ws.send(Message::Text(loud_frame().into())).await.unwrap();
    }
    for _ in 0..40 {
        ws.send(Message::Text(quiet_frame().into())).await.unwrap();
    }
    assert!(
        collect(&mut ws, Duration::from_millis(2600)).await.0.is_empty(),
        "the recognizer has not answered: no \"the line is noisy\""
    );
    h.stt.say("מירושלים").await;
    assert!(!collect(&mut ws, Duration::from_millis(1400)).await.0.is_empty(), "the words get their next question");
    assert!(
        collect(&mut ws, Duration::from_millis(2500)).await.0.is_empty(),
        "and the late empty transcript after them asks nothing"
    );
}

#[tokio::test]
async fn an_empty_transcript_later_than_the_wait_is_noise_at_once() {
    let h = start_server().await;
    let mut ws = open_call(&h, "CA-late-empty").await;
    collect(&mut ws, Duration::from_millis(400)).await;
    h.stt.say("צריך מונית").await;
    while !collect(&mut ws, Duration::from_millis(300)).await.0.is_empty() {}
    *h.stt.slow_final.lock() = Some(Duration::from_millis(2600));
    for _ in 0..20 {
        ws.send(Message::Text(loud_frame().into())).await.unwrap();
    }
    for _ in 0..40 {
        ws.send(Message::Text(quiet_frame().into())).await.unwrap();
    }
    assert!(collect(&mut ws, Duration::from_millis(2300)).await.0.is_empty(), "words may still come");
    assert!(
        !collect(&mut ws, Duration::from_millis(1200)).await.0.is_empty(),
        "nothing came: the question again, without a second two-second wait"
    );
}

#[tokio::test]
async fn asr_progress_extends_the_grace_and_final_words_cancel_noise_recovery() {
    let (h, mut ws) = noise_call().await;
    assert!(collect(&mut ws, Duration::from_millis(1300)).await.0.is_empty());
    let events = h.stt.events.lock().clone().unwrap();
    events.send(SttEvent::Partial("ירוש".into())).await.unwrap();
    assert!(
        collect(&mut ws, Duration::from_millis(1000)).await.0.is_empty(),
        "partial words extend ASR grace beyond the original deadline"
    );
    h.stt.say("מירושלים").await;
    assert!(
        !collect(&mut ws, Duration::from_millis(1400)).await.0.is_empty(),
        "normal recognized speech gets its next question"
    );
    assert!(
        collect(&mut ws, Duration::from_millis(2200)).await.0.is_empty(),
        "no stale watchdog or duplicate question"
    );
}

#[tokio::test]
async fn new_speech_invalidates_the_old_no_words_watchdog() {
    let (h, mut ws) = noise_call().await;
    collect(&mut ws, Duration::from_millis(1300)).await;
    for _ in 0..30 {
        ws.send(Message::Text(loud_frame().into())).await.unwrap();
    }
    assert!(
        collect(&mut ws, Duration::from_millis(1100)).await.0.is_empty(),
        "the old utterance cannot interrupt new speech"
    );
    h.stt.say("מירושלים").await;
    assert!(!collect(&mut ws, Duration::from_millis(500)).await.0.is_empty());
    ws.close(None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(h.telephony.hangups.lock().is_empty(), "caller stop does not generate an assistant hangup");
}

#[tokio::test]
async fn the_dashboard_signs_in_with_its_password_and_the_cookie_opens_the_api() {
    let h = start_server().await;
    let base = format!("http://{}", h.addr);
    let http = reqwest::Client::new();
    let login = |password: &str| http.post(format!("{base}/api/login")).json(&json!({ "password": password })).send();

    assert_eq!(http.get(format!("{base}/api/session")).send().await.unwrap().status(), 401, "not signed in");
    assert_eq!(login("wrong").await.unwrap().status(), 401);

    let ok = login("12345678").await.unwrap();
    assert_eq!(ok.status(), 204);
    let cookie = ok.headers()["set-cookie"].to_str().unwrap().to_string();
    assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"), "{cookie}");
    let session = cookie.split(';').next().unwrap().to_string();

    let me = http.get(format!("{base}/api/session")).header("cookie", &session).send().await.unwrap();
    assert_eq!(me.status(), 200);
    let me: Value = me.json().await.unwrap();
    assert_eq!(me["businesses"][0]["id"], "taxi");
    // Past the check: this test server has no database.
    let calls = http.get(format!("{base}/api/calls")).header("cookie", &session).send().await.unwrap();
    assert_eq!(calls.status(), 503);
    // No WhatsApp service configured: the page says so instead of failing.
    let wa: Value =
        http.get(format!("{base}/api/whatsapp")).header("cookie", &session).send().await.unwrap().json().await.unwrap();
    assert_eq!(wa["configured"], false);
    assert_eq!(http.get(format!("{base}/api/whatsapp")).send().await.unwrap().status(), 401);
    let forged = http.get(format!("{base}/api/calls")).header("cookie", "callora_session=v1.9999999999.x").send();
    assert_eq!(forged.await.unwrap().status(), 401);
    assert_eq!(http.get(format!("{base}/api/nothing")).send().await.unwrap().status(), 404);

    // Guessing stops after five wrong passwords, even the right one then waits.
    for _ in 0..5 {
        login("guess").await.unwrap();
    }
    assert_eq!(login("12345678").await.unwrap().status(), 429);
}

#[tokio::test]
async fn words_begun_before_the_reply_are_sent_as_the_rest_of_the_previous_answer() {
    // A live call: "דוד" ... "אביטבול", said in one breath with a short pause, became the name
    // "דוד" and a driver note "אביטבול": the second part started before the next question.
    let agent = Arc::new(ScriptedAgent::default());
    agent.replies.lock().extend([
        json!({ "action": "none", "fields": [{ "slot": "customer_name", "value": "דוד" }], "say": "יש משהו שהנהג צריך לדעת?", "task": "book_ride" }),
        json!({ "action": "none", "fields": [{ "slot": "customer_name", "value": "דוד אביטבול" }], "say": "יש משהו שהנהג צריך לדעת?", "task": "book_ride" }),
    ]);
    let h = start_server_with(Some(agent.clone())).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}{}", h.addr, twilio::MEDIA_PATH)).await.unwrap();
    let token = twilio::create_stream_token(TOKEN, "CA48", "taxi", 300, chrono_now());
    ws.send(Message::Text(json!({ "event": "connected" }).to_string().into())).await.unwrap();
    ws.send(Message::Text(json!({ "event": "start", "streamSid": "MZ1", "start": { "streamSid": "MZ1", "callSid": "CA48", "customParameters": { "token": token } } }).to_string().into())).await.unwrap();
    collect(&mut ws, Duration::from_millis(400)).await;

    // The caller is talking (the second part of the name starts) ...
    for _ in 0..10 {
        ws.send(Message::Text(loud_frame().into())).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    // ... when the first part's transcript arrives and the agent answers it.
    h.stt.say("דוד").await;
    collect(&mut ws, Duration::from_millis(400)).await;
    h.stt.say("אביטבול").await;
    collect(&mut ws, Duration::from_millis(400)).await;

    let requests = agent.requests.lock();
    assert_eq!(requests.len(), 2);
    assert!(!requests[0].user.contains("OVERLAP"), "the first part is an answer of its own");
    assert!(requests[1].user.contains("OVERLAP"), "the rest of the name: {}", requests[1].user);
}

// ---------------------------------------------------------------------------------------
// Voice experience: barge-in classification, sentence-end protection, gain, continuity.

impl ScriptedStt {
    async fn partial(&self, text: &str) {
        let tx = self.events.lock().clone().expect("stt opened");
        tx.send(SttEvent::Partial(text.into())).await.unwrap();
    }
}

async fn open_call(h: &Harness, sid: &str) -> Ws {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}{}", h.addr, twilio::MEDIA_PATH)).await.unwrap();
    let token = twilio::create_stream_token(TOKEN, sid, "taxi", 300, chrono_now());
    ws.send(Message::Text(json!({ "event": "connected" }).to_string().into())).await.unwrap();
    ws.send(Message::Text(json!({ "event": "start", "streamSid": "MZ1", "start": { "streamSid": "MZ1", "callSid": sid, "customParameters": { "token": token } } }).to_string().into())).await.unwrap();
    ws
}

/// A call whose agent is reading the booking back (a second of live speech, bytes 0x33).
async fn call_at_the_read_back(session: SessionConfig, sid: &str) -> (Harness, Ws) {
    let h = start_server_cfg(None, session).await;
    let mut ws = open_call(&h, sid).await;
    collect(&mut ws, Duration::from_millis(300)).await;
    h.stt.say("צריך מונית עכשיו מרבי עקיבא 12 לנתב\"ג, אנחנו ארבעה").await;
    let (frames, _) = collect(&mut ws, Duration::from_millis(250)).await;
    assert!(frames.contains(&0x33), "the read-back is playing");
    (h, ws)
}

async fn send_frames(ws: &mut Ws, loud: bool, n: usize) {
    for _ in 0..n {
        let f = if loud { loud_frame() } else { quiet_frame() };
        ws.send(Message::Text(f.into())).await.unwrap();
    }
}

/// What a live call looks like: the caller's voice begins, and a moment later the
/// recognizer's first words for it arrive (the session must have seen the voice first).
async fn voice_then_partial(h: &Harness, ws: &mut Ws, text: &str) {
    send_frames(ws, true, 5).await;
    tokio::time::sleep(Duration::from_millis(60)).await;
    h.stt.partial(text).await;
    tokio::time::sleep(Duration::from_millis(60)).await;
}

fn no_gain() -> SessionConfig {
    SessionConfig { tts_gain_db: 0.0, ..SessionConfig::default() }
}

#[tokio::test]
async fn a_backchannel_over_the_agent_does_not_stop_it() {
    let (h, mut ws) = call_at_the_read_back(no_gain(), "CA60").await;
    // "כן" said clearly and loudly for half a second while the details are read back.
    voice_then_partial(&h, &mut ws, "כן").await;
    send_frames(&mut ws, true, 20).await;
    let (frames, clears) = collect(&mut ws, Duration::from_millis(150)).await;
    assert_eq!(clears, 0, "a listening sound does not clear the agent's audio");
    assert!(!frames.is_empty(), "and the read-back goes on");
    // The sound ends: counted as suppressed, not as a barge-in.
    send_frames(&mut ws, false, 40).await;
    collect(&mut ws, Duration::from_millis(100)).await;
    assert_eq!(h.metrics.barge_ins_total.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert_eq!(h.metrics.barge_in_suppressed.get("backchannel"), 1);
}

#[tokio::test]
async fn short_background_words_do_not_stop_the_agent_but_a_real_interruption_does() {
    let (h, mut ws) = call_at_the_read_back(no_gain(), "CA61").await;
    // Words, but only a blink of voice: not yet.
    voice_then_partial(&h, &mut ws, "לא רגע תעצור").await;
    let (_, clears) = collect(&mut ws, Duration::from_millis(60)).await;
    assert_eq!(clears, 0, "words in the first instants are not enough");
    // The voice goes on: now the words count, and the agent stops without waiting a second.
    send_frames(&mut ws, true, 18).await;
    let (_, clears) = collect(&mut ws, Duration::from_millis(150)).await;
    assert!(clears >= 1, "a real interruption clears the agent's audio");
    let m = h.metrics.render();
    assert!(m.contains("callora_barge_in_reason_total{reason=\"words\"} 1"), "{m}");
}

#[tokio::test]
async fn a_loud_voice_that_goes_on_stops_the_agent_before_any_words() {
    let (h, mut ws) = call_at_the_read_back(no_gain(), "CA62").await;
    send_frames(&mut ws, true, 40).await;
    let (_, clears) = collect(&mut ws, Duration::from_millis(150)).await;
    assert!(clears >= 1);
    assert!(h.metrics.render().contains("reason=\"loud_sustained\""));
}

#[tokio::test]
async fn legacy_barge_in_stops_on_the_first_words_as_before() {
    let session =
        SessionConfig { barge: callora_runtime::barge::BargeConfig::legacy(), sentence_end_protect_ms: 0, ..no_gain() };
    let (h, mut ws) = call_at_the_read_back(session, "CA63").await;
    voice_then_partial(&h, &mut ws, "לא רגע").await;
    send_frames(&mut ws, true, 1).await;
    let (_, clears) = collect(&mut ws, Duration::from_millis(150)).await;
    assert!(clears >= 1, "with the old rules a word is enough");
}

#[tokio::test]
async fn an_interruption_near_the_end_of_a_sentence_lets_it_finish() {
    // A generous window: the whole read-back counts as "nearly over".
    let session = SessionConfig { sentence_end_protect_ms: 5_000, ..no_gain() };
    let (h, mut ws) = call_at_the_read_back(session, "CA64").await;
    send_frames(&mut ws, true, 40).await;
    let (frames, clears) = collect(&mut ws, Duration::from_millis(1500)).await;
    assert_eq!(clears, 0, "nothing was cut");
    assert!(frames.iter().filter(|b| **b == 0x33).count() >= 5, "the sentence went on to its end");
    let m = h.metrics.render();
    assert!(m.contains("callora_sentence_end_protected_total 1"), "{m}");
    assert!(m.contains("callora_barge_ins_total 1"), "{m}");
}

#[tokio::test]
async fn gain_makes_the_agent_louder_and_stays_under_the_ceiling() {
    use callora_audio::mulaw::decode;
    let session = SessionConfig { tts_gain_db: 10.0, ..SessionConfig::default() };
    let h = start_server_cfg(None, session).await;
    let mut ws = open_call(&h, "CA65").await;
    let (frames, _) = collect(&mut ws, Duration::from_millis(300)).await;
    assert!(!frames.is_empty());
    let plain = i32::from(decode(0x55)).abs();
    for b in &frames {
        let level = i32::from(decode(*b)).abs();
        assert!(level > plain * 2, "10 dB is about 3x: {level} vs {plain}");
        assert!(level as f32 <= 32124.0 * 10f32.powf(-1.5 / 20.0) * 1.03, "under the ceiling");
    }
}

#[tokio::test]
async fn zero_gain_and_no_trim_leave_the_library_audio_untouched() {
    let session = SessionConfig { tts_gain_db: 0.0, trim_silence: None, ..SessionConfig::default() };
    let h = start_server_cfg(None, session).await;
    let mut ws = open_call(&h, "CA67").await;
    let (frames, _) = collect(&mut ws, Duration::from_millis(300)).await;
    assert_eq!(frames, vec![0x55; 3], "the 3-frame greeting arrives exactly as recorded");
}

#[tokio::test]
async fn the_new_voice_metrics_are_exported() {
    let h = start_server().await;
    let text = h.metrics.render();
    for name in [
        "callora_agent_first_ms",
        "callora_vad_endpoint_ms",
        "callora_caller_speech_ms",
        "callora_audio_gap_ms",
        "callora_barge_in_remaining_ms",
        "callora_barge_in_reason_total",
        "callora_barge_in_suppressed_total",
        "callora_audio_gaps_total",
        "callora_sentence_end_protected_total",
    ] {
        assert!(text.contains(name), "{name} is exported");
    }
}

/// Quiet frames sent one by one until the STT is asked to finalize: how many it took.
async fn quiet_frames_until_finalize(h: &Harness, ws: &mut Ws, max: usize) -> Option<usize> {
    let before = *h.stt.finalizes.lock();
    for n in 1..=max {
        send_frames(ws, false, 1).await;
        tokio::time::sleep(Duration::from_millis(4)).await;
        if *h.stt.finalizes.lock() > before {
            return Some(n);
        }
    }
    None
}

#[tokio::test]
async fn a_finished_short_answer_ends_sooner_and_a_broken_sentence_later() {
    // "כן" to the read-back: the caller is done; 350 ms of quiet are enough (18 frames).
    let (h, mut ws) = call_at_the_read_back(no_gain(), "CA70").await;
    voice_then_partial(&h, &mut ws, "כן").await;
    send_frames(&mut ws, true, 5).await;
    let short = quiet_frames_until_finalize(&h, &mut ws, 40).await.expect("it ends");
    assert!((16..=19).contains(&short), "about 350 ms, got {} ms", short * 20);

    // The same without the adaptation takes the full 500 ms.
    let session = SessionConfig { endpoint_short_ms: 500, endpoint_long_ms: 500, ..no_gain() };
    let (h, mut ws) = call_at_the_read_back(session, "CA71").await;
    voice_then_partial(&h, &mut ws, "כן").await;
    send_frames(&mut ws, true, 5).await;
    let plain = quiet_frames_until_finalize(&h, &mut ws, 40).await.expect("it ends");
    assert!((24..=26).contains(&plain), "500 ms, got {} ms", plain * 20);

    // A sentence that breaks off waits longer for the rest of it.
    let (h, mut ws) = call_at_the_read_back(no_gain(), "CA72").await;
    voice_then_partial(&h, &mut ws, "אני צריך ל").await;
    send_frames(&mut ws, true, 5).await;
    let long = quiet_frames_until_finalize(&h, &mut ws, 60).await.expect("it ends");
    assert!(long >= 34, "about 700 ms, got {} ms", long * 20);
}

#[tokio::test]
async fn consecutive_segments_of_one_reply_play_back_to_back() {
    // Two recordings and a live sentence in one reply: the frames never stop between them.
    let agent = Arc::new(ScriptedAgent::default());
    agent.replies.lock().push_back(
        json!({ "action": "none", "say": "הכל טוב, תודה! מאיפה יוצאים?", "task": "book_ride", "fields": [] }),
    );
    let h = start_server_with(Some(agent)).await;
    let mut ws = open_call(&h, "CA73").await;
    collect(&mut ws, Duration::from_millis(400)).await;
    // The caller speaks (so the pause before the reply is a turn, not a hole in a reply).
    send_frames(&mut ws, true, 10).await;
    send_frames(&mut ws, false, 30).await;
    h.stt.say("מה מצב? אני רוצה להזמין מונית").await;
    let (frames, _) = collect(&mut ws, Duration::from_millis(600)).await;
    assert!(frames.len() >= 6 && frames.iter().all(|b| *b == 0x55));
    let m = h.metrics.render();
    assert!(!m.contains("callora_audio_gaps_total{"), "no silence over the warning size: {m}");
    assert!(h.metrics.audio_gap.count() == 0 || m.contains("callora_audio_gap_ms_bucket"), "{m}");
}

#[tokio::test]
async fn the_recognizer_gets_every_byte_of_the_callers_audio_and_one_finalize_per_utterance() {
    // The caller talks over nothing and then over the agent: nothing the voice changes
    // (gain, barge-in, endpoint, trimming) may touch what the recognizer hears.
    let h = start_server_cfg(None, SessionConfig::default()).await;
    let mut ws = open_call(&h, "CA80").await;
    collect(&mut ws, Duration::from_millis(400)).await;
    let mut sent = Vec::new();
    for (loud, n) in [(true, 25usize), (false, 40), (true, 25), (false, 40)] {
        for _ in 0..n {
            let f = if loud { loud_frame() } else { quiet_frame() };
            let v: Value = serde_json::from_str(&f).unwrap();
            sent.extend(
                base64::engine::general_purpose::STANDARD.decode(v["media"]["payload"].as_str().unwrap()).unwrap(),
            );
            ws.send(Message::Text(f.into())).await.unwrap();
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(*h.stt.audio.lock(), sent, "the recognizer heard exactly what the caller sent");
    assert_eq!(*h.stt.finalizes.lock(), 2, "one finalize for each utterance");
}

#[tokio::test]
async fn a_transcript_that_does_not_stop_the_agent_is_still_answered_not_dropped() {
    let (h, mut ws) = call_at_the_read_back(no_gain(), "CA81").await;
    // A blink of voice, then its words: too little to cut the agent...
    send_frames(&mut ws, true, 5).await;
    tokio::time::sleep(Duration::from_millis(60)).await;
    h.stt.say("לא").await;
    let (_, clears) = collect(&mut ws, Duration::from_millis(150)).await;
    assert_eq!(clears, 0, "the agent is not cut");
    assert_eq!(h.metrics.barge_in_suppressed.get("late_final"), 1);
    // ...and the read-back plays on to its end (the words are handled after it, not lost).
    let (frames, _) = collect(&mut ws, Duration::from_millis(2500)).await;
    assert!(!frames.is_empty(), "the agent finishes what it was saying");
}

/// A second hearing that answers with a street and keeps what it was asked.
#[derive(Default)]
struct ListedHearing {
    asked: Mutex<Vec<(String, Vec<String>)>>,
}

#[async_trait]
impl callora_runtime::ports::Transcriber for ListedHearing {
    async fn transcribe(&self, _mulaw: &[u8], _language: &str, _keyterms: &[String]) -> anyhow::Result<String> {
        anyhow::bail!("the listed hearing is asked with its question")
    }
    async fn hear(&self, _mulaw: &[u8], _language: &str, question: &str, names: &[String]) -> anyhow::Result<String> {
        self.asked.lock().push((question.to_string(), names.to_vec()));
        Ok("רבן יוחנן בן זכאי 45".into())
    }
}

async fn speak(ws: &mut Ws, h: &Harness, text: &str) {
    for _ in 0..30 {
        ws.send(Message::Text(loud_frame().into())).await.unwrap();
    }
    for _ in 0..40 {
        ws.send(Message::Text(quiet_frame().into())).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.stt.say(text).await;
}

#[tokio::test]
async fn a_garbled_street_is_heard_again_against_the_citys_streets() {
    // The call of 2026-10-07: "בן זכאי ארבעים וחמש" came out "ב... זה קח ארבעים וחמש". An audio
    // model told the streets of אלעד wrote it right; the agent gets both hearings.
    let agent = Arc::new(ScriptedAgent::default());
    agent.replies.lock().extend([
        json!({ "say": "איפה באלעד לאסוף?", "action": "none", "task": "book_ride", "asks": ["pickup"],
                "fields": [{ "slot": "pickup", "value": "אלעד" }] }),
        json!({ "say": "לאן?", "action": "none", "task": "book_ride",
                "fields": [{ "slot": "pickup", "value": "רבן יוחנן בן זכאי 45, אלעד" }] }),
    ]);
    let gazetteer = Arc::new(callora_core::gazetteer::Gazetteer::from_tsv(
        "1309\tאלעד\t110\tרבן יוחנן בן זכאי\tofficial\n1309\tאלעד\t110\tבן זכאי\tsynonym\n\
         1309\tאלעד\t111\tרבי עקיבא\tofficial\n6100\tבני ברק\t825\tעזרא\tofficial\n",
    ));
    let hearing = Arc::new(ListedHearing::default());
    let h = start_server_inner(
        Some(agent.clone()),
        SessionConfig { tts_gain_db: 0.0, ..SessionConfig::default() },
        None,
        Default::default(),
        Some((gazetteer, hearing.clone())),
    )
    .await;
    let mut ws = open_call(&h, "CA-heard-again").await;
    collect(&mut ws, Duration::from_millis(400)).await;
    speak(&mut ws, &h, "צריך מונית מאלעד").await;
    while !collect(&mut ws, Duration::from_millis(400)).await.0.is_empty() {}
    assert!(hearing.asked.lock().is_empty(), "a city answer that names a town is not heard again");

    speak(&mut ws, &h, "ב... זה קח ארבעים וחמש").await;
    collect(&mut ws, Duration::from_millis(600)).await;
    let asked = hearing.asked.lock().clone();
    assert_eq!(asked.len(), 1, "the street answer is heard again");
    assert_eq!(asked[0].0, "which street in אלעד");
    assert!(
        asked[0].1.contains(&"רבן יוחנן בן זכאי".to_string()) && asked[0].1.contains(&"רבי עקיבא".to_string()),
        "{:?}",
        asked[0].1
    );
    assert!(!asked[0].1.contains(&"עזרא".to_string()), "only that city's streets");
    let requests = agent.requests.lock();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1].user.contains("SECOND HEARING") && requests[1].user.contains("רבן יוחנן בן זכאי 45"),
        "the agent gets the second hearing: {}",
        requests[1].user
    );
}

#[tokio::test]
async fn a_street_the_stream_heard_right_is_not_heard_again() {
    let agent = Arc::new(ScriptedAgent::default());
    agent.replies.lock().extend([
        json!({ "say": "איפה באלעד לאסוף?", "action": "none", "task": "book_ride", "asks": ["pickup"],
                "fields": [{ "slot": "pickup", "value": "אלעד" }] }),
        json!({ "say": "לאן?", "action": "none", "task": "book_ride",
                "fields": [{ "slot": "pickup", "value": "בן זכאי 45, אלעד" }] }),
    ]);
    let gazetteer = Arc::new(callora_core::gazetteer::Gazetteer::from_tsv(
        "1309\tאלעד\t110\tרבן יוחנן בן זכאי\tofficial\n1309\tאלעד\t110\tבן זכאי\tsynonym\n",
    ));
    let hearing = Arc::new(ListedHearing::default());
    let h = start_server_inner(
        Some(agent.clone()),
        SessionConfig { tts_gain_db: 0.0, ..SessionConfig::default() },
        None,
        Default::default(),
        Some((gazetteer, hearing.clone())),
    )
    .await;
    let mut ws = open_call(&h, "CA-heard-once").await;
    collect(&mut ws, Duration::from_millis(400)).await;
    speak(&mut ws, &h, "צריך מונית מאלעד").await;
    while !collect(&mut ws, Duration::from_millis(400)).await.0.is_empty() {}
    speak(&mut ws, &h, "בן זכאי ארבעים וחמש").await;
    collect(&mut ws, Duration::from_millis(600)).await;
    assert!(hearing.asked.lock().is_empty(), "no second of waiting for a street already found");
    assert_eq!(agent.requests.lock().len(), 2);
}

#[tokio::test]
async fn the_agents_own_words_coming_back_are_not_an_answer() {
    // On a speakerphone the agent's voice reaches the caller's microphone: "לאן בבני ברק?"
    // came back as the caller's words.
    let agent = Arc::new(ScriptedAgent::default());
    agent.replies.lock().extend([
        json!({ "say": "לאן בבני ברק נוסעים?", "action": "none", "task": "book_ride",
                "fields": [{ "slot": "destination", "value": "בני ברק" }] }),
        json!({ "say": "כמה נוסעים?", "action": "none", "task": "book_ride",
                "fields": [{ "slot": "destination", "value": "רבי עקיבא 2, בני ברק" }] }),
    ]);
    let h = start_server_with(Some(agent.clone())).await;
    let mut ws = open_call(&h, "CA-echo").await;
    collect(&mut ws, Duration::from_millis(400)).await;
    speak(&mut ws, &h, "צריך מונית לבני ברק").await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    // While the reply plays, its own words come back.
    speak(&mut ws, &h, "לאן בבני ברק נוסעים").await;
    while !collect(&mut ws, Duration::from_millis(400)).await.0.is_empty() {}
    assert_eq!(agent.requests.lock().len(), 1, "the echo is not sent to the agent");
    // A real answer that repeats the city is the caller's.
    speak(&mut ws, &h, "בבני ברק, רבי עקיבא שתיים").await;
    collect(&mut ws, Duration::from_millis(600)).await;
    assert_eq!(agent.requests.lock().len(), 2, "the answer is");
}

fn timed_frame(audio: &[u8], at_ms: u64) -> String {
    json!({ "event": "media", "streamSid": "MZ1", "media": { "track": "inbound", "timestamp": at_ms.to_string(),
            "payload": base64::engine::general_purpose::STANDARD.encode(audio) } })
    .to_string()
}

#[tokio::test]
async fn words_said_while_the_line_cut_out_are_told_to_the_agent() {
    let agent = Arc::new(ScriptedAgent::default());
    agent.replies.lock().extend([json!({ "say": "מאיפה לאן?", "action": "none", "task": "book_ride", "fields": [] })]);
    let h = start_server_with(Some(agent.clone())).await;
    let mut ws = open_call(&h, "CA-line").await;
    collect(&mut ws, Duration::from_millis(400)).await;
    let loud: Vec<u8> = (0..160).map(|i| if i % 2 == 0 { 0x10 } else { 0x90 }).collect();
    let mut at = 0;
    for i in 0..60 {
        if i == 20 {
            at += 400; // 400 ms of the caller's audio never arrived
        }
        let audio = if i < 30 { loud.clone() } else { vec![0xFF; 160] };
        ws.send(Message::Text(timed_frame(&audio, at).into())).await.unwrap();
        at += 20;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.stt.say("אני רוצה מונית מבאר").await;
    collect(&mut ws, Duration::from_millis(500)).await;
    let requests = agent.requests.lock();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].user.contains("BAD LINE") && requests[0].user.contains("400 ms"), "{}", requests[0].user);
}

#[tokio::test]
async fn a_paused_i_want_is_the_start_of_a_request_not_noise() {
    // The call of 16:09: "אני רוצה." was dropped as noise, and the line was silent five seconds.
    let agent = Arc::new(ScriptedAgent::default());
    agent.replies.lock().extend([
        json!({ "say": "מאיפה לאן?", "action": "none", "task": "book_ride", "fields": [] }),
        json!({ "say": "מאיפה לאן?", "action": "none", "task": "book_ride", "fields": [] }),
    ]);
    let h = start_server_with(Some(agent.clone())).await;
    let mut ws = open_call(&h, "CA-i-want").await;
    collect(&mut ws, Duration::from_millis(400)).await;
    // The rest follows: one request with both.
    h.stt.say("אני רוצה.").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    h.stt.say("להזמין מונית").await;
    collect(&mut ws, Duration::from_millis(600)).await;
    {
        let requests = agent.requests.lock();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].user.contains("אני רוצה. להזמין מונית"), "{}", requests[0].user);
    }
    // Nothing follows: answered as it is, at once after the short wait, not dropped.
    h.stt.say("אני רוצה.").await;
    let (frames, _) = collect(&mut ws, Duration::from_millis(2000)).await;
    assert_eq!(agent.requests.lock().len(), 2, "answered");
    assert!(!frames.is_empty(), "something is said");
}

/// An agent that thinks this long before its first words.
struct SlowAgent(Duration, Arc<ScriptedAgent>);

#[async_trait]
impl LanguageModel for SlowAgent {
    async fn extract(&self, request: &LlmRequest) -> anyhow::Result<Value> {
        self.1.extract(request).await
    }
    async fn stream(&self, request: &LlmRequest) -> anyhow::Result<TextStream> {
        tokio::time::sleep(self.0).await;
        self.1.stream(request).await
    }
    fn name(&self) -> &'static str {
        "slow-agent"
    }
}

#[tokio::test]
async fn a_slow_agent_is_heard_thinking_but_not_every_turn() {
    // "שנייה, שנייה, שנייה": said on every turn at 0.3 s, the thinking sound sounded like a stall.
    // Now only when the agent is still silent 1.5 s after the caller's words, once a turn and not
    // two turns in a row.
    let scripted = Arc::new(ScriptedAgent::default());
    scripted.replies.lock().extend([
        json!({ "say": "", "phrase": "ask_route", "action": "none", "task": "book_ride", "fields": [] }),
        json!({ "say": "", "phrase": "ask_route", "action": "none", "task": "book_ride", "fields": [] }),
    ]);
    // The taxi business has it off; a business with it on.
    let mut b: Value = serde_json::from_str(TAXI).unwrap();
    b["agent"]["thinking_filler"] = json!("ack_listening");
    b["agent"]["filler_after_ms"] = json!(1500);
    BUSINESS.with(|cell| *cell.borrow_mut() = Some(b.to_string()));
    let h = start_server_with(Some(Arc::new(SlowAgent(Duration::from_millis(2200), scripted.clone())))).await;
    let mut ws = open_call(&h, "CA-ack").await;
    collect(&mut ws, Duration::from_millis(400)).await;
    h.stt.say("אני רוצה להזמין מונית").await;
    let (frames, _) = collect(&mut ws, Duration::from_millis(1200)).await;
    assert!(frames.is_empty(), "a quick enough agent is not covered");
    let (frames, _) = collect(&mut ws, Duration::from_millis(700)).await;
    assert!(!frames.is_empty(), "a slow one is: the thinking sound");
    while !collect(&mut ws, Duration::from_millis(400)).await.0.is_empty() {}
    h.stt.say("מאלעד, בעוד חצי שעה").await;
    let (frames, _) = collect(&mut ws, Duration::from_millis(1900)).await;
    assert!(frames.is_empty(), "not two turns in a row");
}

#[tokio::test]
async fn the_first_question_is_the_pickup_even_when_the_agent_asks_the_destination() {
    // The call of 17:57: on the first turn the agent asked "לאן צריך להגיע?" before "מאיפה?".
    let agent = Arc::new(ScriptedAgent::default());
    agent.replies.lock().extend([json!({ "action": "none", "fields": [], "asks": ["destination"],
        "say": "לאן צריך להגיע?", "task": "book_ride" })]);
    let h = start_server_with(Some(agent.clone())).await;
    let mut ws = open_call(&h, "CA-order-first").await;
    collect(&mut ws, Duration::from_millis(400)).await;
    h.stt.say("זה רוצה בני ברק").await;
    collect(&mut ws, Duration::from_millis(800)).await;
    let records = RECORDS.lock();
    let id = records
        .iter()
        .find_map(|r| match r {
            CallRecord::Started { info } if info.call_sid == "CA-order-first" => Some(info.call_id),
            _ => None,
        })
        .expect("the call");
    let said: Vec<&str> = records
        .iter()
        .filter_map(|r| match r {
            CallRecord::Turn { call_id, speaker, text, .. } if *call_id == id && speaker == "agent" => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect();
    let last = said.last().copied().unwrap_or("");
    assert!(
        !last.contains("לאן") && (last.contains("לאסוף") || last.contains("מאיפה") || last.contains("אוספים")),
        "{said:?}"
    );
}

#[tokio::test]
async fn an_answer_in_the_agents_own_words_after_it_spoke_is_kept() {
    // "בבני ברק" to "לאיזה רחוב בבני ברק?" is the caller's, not an echo.
    let agent = Arc::new(ScriptedAgent::default());
    agent.replies.lock().extend([
        json!({ "say": "לאיזה רחוב בבני ברק?", "action": "none", "task": "book_ride",
                "fields": [{ "slot": "destination", "value": "בני ברק" }] }),
        json!({ "say": "איזה רחוב?", "action": "none", "task": "book_ride", "fields": [] }),
    ]);
    let h = start_server_with(Some(agent.clone())).await;
    let mut ws = open_call(&h, "CA-own-words").await;
    collect(&mut ws, Duration::from_millis(400)).await;
    speak(&mut ws, &h, "צריך מונית לבני ברק").await;
    while !collect(&mut ws, Duration::from_millis(400)).await.0.is_empty() {}
    speak(&mut ws, &h, "בבני ברק").await;
    collect(&mut ws, Duration::from_millis(600)).await;
    assert_eq!(agent.requests.lock().len(), 2, "the answer reaches the agent");
}

#[tokio::test]
async fn the_same_question_again_says_first_it_did_not_catch_the_answer() {
    // 15 of 74 recorded calls heard the very same question twice in a row.
    let agent = Arc::new(ScriptedAgent::default());
    agent.replies.lock().extend([
        json!({ "action": "none", "fields": [{ "slot": "pickup", "value": "בן זכאי 45, אלעד" },
                { "slot": "destination", "value": "רבי עקיבא 2, בני ברק" }], "asks": ["passengers"],
                "say": "כמה נוסעים?", "task": "book_ride" }),
        json!({ "action": "none", "fields": [], "asks": ["passengers"], "say": "כמה נוסעים?", "task": "book_ride" }),
    ]);
    let h = start_server_with(Some(agent.clone())).await;
    let mut ws = open_call(&h, "CA-same-q").await;
    collect(&mut ws, Duration::from_millis(400)).await;
    h.stt.say("אני רוצה מונית מבן זכאי 45 באלעד לרבי עקיבא 2 בבני ברק").await;
    while !collect(&mut ws, Duration::from_millis(400)).await.0.is_empty() {}
    h.stt.say("אהרון").await;
    collect(&mut ws, Duration::from_millis(800)).await;
    let records = RECORDS.lock();
    let id = records
        .iter()
        .find_map(|r| match r {
            CallRecord::Started { info } if info.call_sid == "CA-same-q" => Some(info.call_id),
            _ => None,
        })
        .expect("the call");
    let said: Vec<&str> = records
        .iter()
        .filter_map(|r| match r {
            CallRecord::Turn { call_id, speaker, text, .. } if *call_id == id && speaker == "agent" => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect();
    let end = said[said.len().saturating_sub(2)..].join(" ");
    assert!(end.contains("לא שמעתי טוב. כמה נוסעים"), "{said:?}");
}

/// A second hearing that hears a number and keeps what it was asked.
#[derive(Default)]
struct NumberHearing {
    asked: Mutex<Vec<(String, Vec<String>)>>,
}

#[async_trait]
impl callora_runtime::ports::Transcriber for NumberHearing {
    async fn transcribe(&self, _mulaw: &[u8], _language: &str, _keyterms: &[String]) -> anyhow::Result<String> {
        anyhow::bail!("asked with its question")
    }
    async fn hear(&self, _mulaw: &[u8], _language: &str, question: &str, names: &[String]) -> anyhow::Result<String> {
        self.asked.lock().push((question.to_string(), names.to_vec()));
        Ok("שתיים".into())
    }
}

#[tokio::test]
async fn a_passengers_answer_with_no_number_is_heard_again() {
    // The call of 18:56: "שתיים" to "כמה נוסעים?" was heard "ביי." and the call hung up.
    let agent = Arc::new(ScriptedAgent::default());
    agent.replies.lock().extend([
        json!({ "action": "none", "fields": [{ "slot": "pickup", "value": "בן זכאי 45, אלעד" },
                { "slot": "destination", "value": "רבי עקיבא 2, בני ברק" }], "asks": ["passengers"],
                "say": "כמה נוסעים?", "task": "book_ride" }),
        json!({ "action": "none", "fields": [{ "slot": "passengers", "value": "2" }], "asks": ["customer_name"],
                "say": "על שם מי?", "task": "book_ride" }),
    ]);
    let gazetteer = Arc::new(callora_core::gazetteer::Gazetteer::from_tsv(
        "1309\tאלעד\t110\tרבן יוחנן בן זכאי\tofficial\n1309\tאלעד\t110\tבן זכאי\tsynonym\n\
         6100\tבני ברק\t301\tרבי עקיבא\tofficial\n",
    ));
    let hearing = Arc::new(NumberHearing::default());
    let h = start_server_inner(
        Some(agent.clone()),
        SessionConfig { tts_gain_db: 0.0, ..SessionConfig::default() },
        None,
        Default::default(),
        Some((gazetteer, hearing.clone())),
    )
    .await;
    let mut ws = open_call(&h, "CA-two").await;
    collect(&mut ws, Duration::from_millis(400)).await;
    speak(&mut ws, &h, "מבן זכאי 45 באלעד לרבי עקיבא 2 בבני ברק").await;
    while !collect(&mut ws, Duration::from_millis(400)).await.0.is_empty() {}
    speak(&mut ws, &h, "ביי.").await;
    collect(&mut ws, Duration::from_millis(600)).await;
    let asked = hearing.asked.lock().clone();
    assert_eq!(asked.len(), 1, "heard again");
    assert!(asked[0].0.contains("passengers") && asked[0].1.contains(&"שתיים".to_string()), "{asked:?}");
    let requests = agent.requests.lock();
    assert!(requests[1].user.contains("SECOND HEARING") && requests[1].user.contains("שתיים"), "{}", requests[1].user);
}

#[tokio::test]
async fn words_the_failed_recognizer_never_answered_go_to_the_next_one() {
    // OpenAI out of credit: the session closed (failed over to the backup) with the caller's last
    // words never transcribed. The new session gets them and is asked to finish them.
    let h = start_server().await;
    let mut ws = open_call(&h, "CA-failover").await;
    collect(&mut ws, Duration::from_millis(400)).await;
    *h.stt.slow_final.lock() = Some(Duration::from_secs(10));
    for _ in 0..30 {
        ws.send(Message::Text(loud_frame().into())).await.unwrap();
    }
    for _ in 0..40 {
        ws.send(Message::Text(quiet_frame().into())).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(*h.stt.finalizes.lock(), 1);
    let before = h.stt.audio.lock().len();
    let events = h.stt.events.lock().clone().unwrap();
    events.send(SttEvent::Closed).await.unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(*h.stt.finalizes.lock(), 2, "the new session is asked to finish the words");
    assert!(h.stt.audio.lock().len() >= before + 30 * 160, "with the words' audio");
}

#[tokio::test]
async fn a_pickup_not_found_is_sent_from_the_callers_phone() {
    let agent = Arc::new(ScriptedAgent::default());
    agent.replies.lock().extend([
        json!({ "action": "none", "fields": [{ "slot": "pickup", "value": "אלעד" }], "asks": ["pickup"], "say": "איפה באלעד לאסוף?", "task": "book_ride" }),
        json!({ "action": "none", "fields": [{ "slot": "pickup", "value": "עריף 12, אלעד" }], "asks": ["pickup"], "say": "", "task": "book_ride" }),
        json!({ "action": "none", "fields": [{ "slot": "pickup", "value": "עריפים 12, אלעד" }], "asks": ["pickup"], "say": "", "task": "book_ride" }),
    ]);
    let gazetteer = Arc::new(callora_core::gazetteer::Gazetteer::from_tsv(
        "1309	אלעד	110	רבן יוחנן בן זכאי	official
",
    ));
    LINKS.with(|l| l.set(true));
    let h = start_server_inner(
        Some(agent.clone()),
        SessionConfig { tts_gain_db: 0.0, ..SessionConfig::default() },
        None,
        Default::default(),
        Some((gazetteer, Arc::new(ListedHearing::default()))),
    )
    .await;
    // The voice webhook first: the call knows the caller's number to text.
    let mut params = BTreeMap::new();
    params.insert("CallSid".to_string(), "CA-location".to_string());
    params.insert("To".to_string(), NUMBER.to_string());
    params.insert("From".to_string(), "+972501111111".to_string());
    let sig = twilio::signature(TOKEN, &format!("https://calls.example.test{}", twilio::VOICE_PATH), &params);
    let url = format!("http://{}{}", h.addr, twilio::VOICE_PATH);
    let ok = reqwest::Client::new().post(&url).header("X-Twilio-Signature", sig).form(&params).send().await.unwrap();
    assert_eq!(ok.status(), 200);
    let mut ws = open_call(&h, "CA-location").await;
    collect(&mut ws, Duration::from_millis(400)).await;
    for said in ["צריך מונית מאלעד", "עריף שתים עשרה", "עריפים שתים עשרה"] {
        h.stt.say(said).await;
        while !collect(&mut ws, Duration::from_millis(400)).await.0.is_empty() {}
    }
    let sms = h.telephony.sms.lock().clone();
    assert_eq!(sms.len(), 1, "one link texted, once a call: {sms:?}");
    let url = sms[0].1.split_whitespace().last().unwrap().to_string();
    assert!(url.starts_with("https://calls.example.test/l/"), "{url}");
    let path = url.trim_start_matches("https://calls.example.test");
    let client = reqwest::Client::new();
    let page = client.get(format!("http://{}{path}", h.addr)).send().await.unwrap();
    assert_eq!(page.status(), 200);
    let posted = client
        .post(format!("http://{}{path}", h.addr))
        .json(&json!({ "lat": 32.04, "lon": 34.95 }))
        .send()
        .await
        .unwrap();
    assert_eq!(posted.status(), 204);
    collect(&mut ws, Duration::from_millis(600)).await;
    let bad = client
        .post(format!("http://{}/l/nope", h.addr))
        .json(&json!({ "lat": 32.0, "lon": 34.0 }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 404);
    let records = RECORDS.lock();
    let id = records
        .iter()
        .find_map(|r| match r {
            CallRecord::Started { info } if info.call_sid == "CA-location" => Some(info.call_id),
            _ => None,
        })
        .expect("the call");
    let said: Vec<&str> = records
        .iter()
        .filter_map(|r| match r {
            CallRecord::Turn { call_id, speaker, text, .. } if *call_id == id && speaker == "agent" => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect();
    assert!(said.iter().any(|t| t.contains("קיבלתי את המיקום")), "{said:?}");
}

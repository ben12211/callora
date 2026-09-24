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
use callora_core::render::library_entries;
use callora_runtime::actions::ConfiguredActions;
use callora_runtime::metrics::Metrics;
use callora_runtime::ports::{NoWhisper, NullStore, SpeechToText, SttEvent, SttInput, SttSession, Telephony};
use callora_runtime::server::{router, AppState, ServerSettings};
use callora_runtime::session::{Services, SessionConfig};
use callora_runtime::twilio;

const TAXI: &str = include_str!("../../../businesses/taxi.json");
const NUMBER: &str = "+972500000000";
const TOKEN: &str = "test-auth-token";

/// STT whose transcripts the test pushes.
#[derive(Default, Clone)]
struct ScriptedStt {
    events: Arc<Mutex<Option<mpsc::Sender<SttEvent>>>>,
    finalizes: Arc<Mutex<usize>>,
}

#[async_trait]
impl SpeechToText for ScriptedStt {
    async fn open(&self, _language: &str) -> anyhow::Result<SttSession> {
        let (in_tx, mut in_rx) = mpsc::channel(1024);
        let (ev_tx, ev_rx) = mpsc::channel(16);
        *self.events.lock() = Some(ev_tx);
        let finalizes = self.finalizes.clone();
        tokio::spawn(async move {
            while let Some(i) = in_rx.recv().await {
                if i == SttInput::Finalize {
                    *finalizes.lock() += 1;
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
}

struct Harness {
    addr: std::net::SocketAddr,
    stt: ScriptedStt,
    telephony: Arc<FakeTelephony>,
    metrics: Arc<Metrics>,
}

async fn start_server() -> Harness {
    let env = |k: &str| {
        (k == "TAXI_PHONE_NUMBERS")
            .then(|| NUMBER.to_string())
            .or_else(|| (k == "ELEVENLABS_VOICE_ID").then(|| "voice".into()))
    };
    let business = Business::from_json(TAXI, "taxi.json", &env).unwrap();
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
        tts: Some(Arc::new(FakeTts)),
        tts_cache: TtsCache::new(100),
        actions: Arc::new(ConfiguredActions::new(reqwest::Client::new(), HashMap::new())),
        telephony: telephony.clone(),
        store: Arc::new(NullStore),
        whisper: Arc::new(NoWhisper),
        metrics: metrics.clone(),
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
    };
    let state = AppState::new(registry, libraries, services, SessionConfig::default(), settings, None);
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

    // The caller talks over the read-back: audio stops and Twilio is told to clear.
    for _ in 0..8 {
        ws.send(Message::Text(loud_frame().into())).await.unwrap();
    }
    let (_, clears) = collect(&mut ws, Duration::from_millis(150)).await;
    assert!(clears >= 1, "barge-in clears Twilio's buffer");
    for _ in 0..25 {
        ws.send(Message::Text(quiet_frame().into())).await.unwrap();
    }
    let (frames, _) = collect(&mut ws, Duration::from_millis(200)).await;
    assert!(frames.is_empty(), "nothing plays after the barge-in");
    assert!(*h.stt.finalizes.lock() >= 1, "the VAD endpoint asked the STT to finalize");

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

fn chrono_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

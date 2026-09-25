//! Providers against local mock servers: the OpenAI-compatible understanding path and the
//! ElevenLabs voice-library build, end to end, without network access or keys.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

use callora_audio::library::{LibraryBuilder, VoiceLibrary};
use callora_audio::tts::Synthesizer;
use callora_core::business::Business;
use callora_core::engine::Engine;
use callora_core::llm::{build_request, parse_response};
use callora_core::render::library_entries;
use callora_core::understanding::{fast_path, merge};
use callora_core::values::SlotValue;
use callora_providers::elevenlabs::ElevenLabs;
use callora_providers::openai::OpenAi;
use callora_runtime::ports::LanguageModel;

const TAXI: &str = include_str!("../../../businesses/taxi.json");

fn taxi() -> Arc<Business> {
    Arc::new(
        Business::from_json(TAXI, "taxi.json", &|k| (k == "ELEVENLABS_VOICE_ID").then(|| "voice-1".to_string()))
            .unwrap(),
    )
}

async fn serve(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

#[tokio::test]
async fn llm_extraction_fills_what_the_rules_could_not() {
    // The model answers as instructed: strict JSON in the message content.
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|Json(req): Json<Value>| async move {
            assert_eq!(req["response_format"]["type"], "json_schema");
            assert_eq!(req["response_format"]["json_schema"]["strict"], true);
            let content = json!({
                "meta_intent": null,
                "intent": "book_ride",
                "intent_confidence": 0.93,
                "affirm": null,
                "frustrated": false,
                "slots": [
                    { "slot": "pickup", "value": "ז'בוטינסקי פינת רבי עקיבא", "confidence": 0.8 },
                    { "slot": "destination", "value": "נמל התעופה", "confidence": 0.9 },
                    { "slot": "passengers", "value": "שלושה", "confidence": 0.9 },
                    { "slot": "not_a_slot", "value": "x", "confidence": 1.0 }
                ]
            });
            Json(json!({ "choices": [ { "message": { "content": content.to_string() } } ] }))
        }),
    );
    let addr = serve(app).await;
    let llm = OpenAi::new(reqwest::Client::new(), "test-key".into(), Some(format!("http://{addr}/v1")), None);

    let b = taxi();
    let mut engine = Engine::new(b.clone(), 1);
    engine.start();
    let text = "תקשיב אחי אני על ז'בוטינסקי פינת רבי עקיבא וצריך להגיע לשדה, שלושה אנשים";
    let (fast, _needs_llm) = fast_path(&b, &engine.context(), text);
    let request = build_request(&b, &engine.context(), &engine.state, text);
    let reply = llm.extract(&request).await.unwrap();
    let u = merge(fast, parse_response(&b, &engine.context(), text, &reply));

    assert_eq!(u.intent.as_ref().unwrap().id, "book_ride");
    assert!(u.slot("not_a_slot").is_none(), "unknown slots are dropped");
    match &u.slot("destination").unwrap().value {
        SlotValue::Place { spoken, address, .. } => {
            assert_eq!(spoken, "נתב״ג", "LLM values go through the same gazetteer");
            assert!(address.is_some());
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(u.slot("passengers").unwrap().value, SlotValue::Integer { value: 3 });
    engine.on_utterance(u);
    assert_eq!(engine.state.run.as_ref().unwrap().pipeline, "book_ride");
}

#[tokio::test]
async fn llm_errors_are_reported_not_invented() {
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            (axum::http::StatusCode::TOO_MANY_REQUESTS, Json(json!({ "error": { "message": "rate limited" } })))
        }),
    );
    let addr = serve(app).await;
    let llm = OpenAi::new(reqwest::Client::new(), "k".into(), Some(format!("http://{addr}/v1")), None);
    let b = taxi();
    let engine = Engine::new(b.clone(), 1);
    let err = llm.extract(&build_request(&b, &engine.context(), &engine.state, "שלום")).await.unwrap_err();
    assert!(err.to_string().contains("rate limited"));
}

#[derive(Clone, Default)]
struct Calls(Arc<AtomicUsize>);

#[tokio::test]
async fn voice_library_builds_incrementally_and_loads() {
    let calls = Calls::default();
    let app = Router::new()
        .route(
            "/v1/text-to-speech/{voice}/stream",
            post(
                |State(calls): State<Calls>,
                 Path(voice): Path<String>,
                 headers: axum::http::HeaderMap,
                 Json(body): Json<Value>| async move {
                    assert_eq!(voice, "voice-1");
                    assert_eq!(headers.get("xi-api-key").unwrap(), "el-key");
                    assert_eq!(body["model_id"], "eleven_v3");
                    assert!(
                        !body["text"].as_str().unwrap().chars().any(|c| c.is_ascii_digit()),
                        "numbers are spoken as words: {}",
                        body["text"]
                    );
                    calls.0.fetch_add(1, Ordering::SeqCst);
                    // Stream a few chunks of "audio".
                    let chunks: Vec<Result<Vec<u8>, std::io::Error>> = vec![Ok(vec![0x42; 100]), Ok(vec![0x42; 60])];
                    Body::from_stream(futures::stream::iter(chunks))
                },
            ),
        )
        .with_state(calls.clone());
    let addr = serve(app).await;
    let synth: Arc<dyn Synthesizer> =
        Arc::new(ElevenLabs::new(reqwest::Client::new(), "el-key".into(), Some(format!("http://{addr}"))));

    let b = taxi();
    let dir = std::env::temp_dir().join(format!("callora-lib-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let builder = LibraryBuilder {
        business: &b,
        synthesizer: synth.clone(),
        voice_id: "voice-1".into(),
        model: "eleven_v3".into(),
        root: dir.clone(),
        concurrency: 8,
    };
    let first = builder.build().await.unwrap();
    let expected = library_entries(&b).len();
    assert_eq!(first.total, expected);
    assert_eq!(first.generated, expected);
    assert!(first.failed.is_empty());
    assert_eq!(calls.0.load(Ordering::SeqCst), expected);

    let second = builder.build().await.unwrap();
    assert_eq!(second.generated, 0, "unchanged clips are reused");
    assert_eq!(second.reused, expected);
    assert_eq!(calls.0.load(Ordering::SeqCst), expected);

    let lib = VoiceLibrary::load(&dir, &b).unwrap();
    assert_eq!(lib.len(), expected);
    let clip = lib.get("normal", "אהלן, איך אפשר לעזור?").expect("greeting clip");
    assert_eq!(clip.len(), 160);
    assert!(lib.get("slow", "מאיפה לאסוף?").is_some());

    // A library generated for another voice is not used.
    let other = Arc::new(
        Business::from_json(TAXI, "taxi.json", &|k| (k == "ELEVENLABS_VOICE_ID").then(|| "voice-2".to_string()))
            .unwrap(),
    );
    assert!(VoiceLibrary::load(&dir, &other).unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

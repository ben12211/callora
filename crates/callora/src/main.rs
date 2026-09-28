//! `callora`: the phone agent server and the operator commands around it.

use std::collections::HashMap;
use std::io::{BufRead, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use clap::{Parser, Subcommand};

use callora_audio::library::{LibraryBuilder, VoiceLibrary};
use callora_audio::tts::{Synthesizer, TtsCache};
use callora_core::business::{Business, BusinessRegistry};
use callora_core::engine::{Directive, Engine};
use callora_core::render::library_entries;
use callora_core::understanding::{fast_path, merge};
use callora_providers::{
    cartesia::Cartesia,
    elevenlabs::ElevenLabs,
    openai::OpenAi,
    race::{FirstAnswer, Hedged},
    scribe::Scribe,
    twilio_rest::TwilioRest,
};
use callora_runtime::actions::ConfiguredActions;
use callora_runtime::metrics::Metrics;
use callora_runtime::ports::{
    ActionRunner, CallInfo, CallStore, LanguageModel, NoWhisper, NullStore, SpeechToText, SttSession, Telephony,
};
use callora_runtime::server::{router, AppState, ServerSettings};
use callora_runtime::session::{Services, SessionConfig};

mod eval;

#[derive(Parser)]
#[command(name = "callora", version, about = "Callora V2 phone agent")]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// Directory of Business JSON files.
    #[arg(long, env = "BUSINESS_CONFIG_DIR", default_value = "businesses", global = true)]
    businesses: PathBuf,
}

#[derive(Subcommand)]
enum Command {
    /// Run the phone agent server.
    Serve,
    /// Apply database migrations and exit.
    Migrate,
    /// Probe the local server's /health (used by the container healthcheck).
    Healthcheck,
    /// Business configuration tools.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Pre-generated voice library tools.
    VoiceLibrary {
        #[command(subcommand)]
        command: LibraryCommand,
    },
    /// Talk to a business in text, through the real engine and planner.
    Simulate {
        #[arg(long, default_value = "taxi")]
        business: String,
    },
    /// Print how one utterance is understood.
    Understand {
        #[arg(long, default_value = "taxi")]
        business: String,
        text: String,
    },
    /// Print the agent's system prompt and reply schema, as JSON.
    AgentPrompt {
        #[arg(long, default_value = "taxi")]
        business: String,
    },
    /// Run the agent on the recorded conversations with the real model and report the pass
    /// rate, the latency and the cost (see evaluation/README.md).
    Eval {
        /// A case file or a directory of them.
        #[arg(long, default_value = "evaluation/agent")]
        cases: PathBuf,
        /// A model to test; repeat to compare. Without one, the production agent (its model
        /// and hedge, from AGENT_MODEL / AGENT_BACKUP_MODEL / AGENT_HEDGE_MS).
        #[arg(long = "model")]
        models: Vec<String>,
        /// Reasoning effort for the models given (default AGENT_REASONING_EFFORT, else none).
        #[arg(long)]
        reasoning: Option<String>,
        /// Runs per case: the model is not deterministic.
        #[arg(long, default_value_t = 3)]
        repeat: usize,
        /// Only the cases whose id contains this.
        #[arg(long)]
        only: Option<String>,
        /// Cases running at once (mind the account's rate limits).
        #[arg(long, default_value_t = 4)]
        concurrency: usize,
        /// Also write the full report (every reply, every failure) as JSON.
        #[arg(long)]
        json: Option<PathBuf>,
        /// Only check that the case files are well formed; no model is called.
        #[arg(long)]
        check: bool,
        /// Exit with status 1 when a model's pass rate is below this (0 to 1).
        #[arg(long)]
        min_pass: Option<f64>,
        /// At most this many model requests a minute (a free-tier key allows only a few;
        /// past it every request fails with 429 and the cases fail for nothing).
        #[arg(long)]
        rpm: Option<u32>,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Validate every business file (exit code 1 on any problem).
    Validate,
}

#[derive(Subcommand)]
enum LibraryCommand {
    /// Generate every missing clip with ElevenLabs.
    Build {
        /// One business; every business when omitted.
        #[arg(long)]
        business: Option<String>,
        #[arg(long, env = "AUDIO_LIBRARY_DIR", default_value = "voice-library")]
        out: PathBuf,
        #[arg(long, default_value_t = 4)]
        concurrency: usize,
        /// List what would be generated without calling ElevenLabs.
        #[arg(long)]
        dry_run: bool,
    },
    /// Show how much of each business's library exists.
    Status {
        #[arg(long, env = "AUDIO_LIBRARY_DIR", default_value = "voice-library")]
        dir: PathBuf,
    },
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// The understanding LLM. With both GEMINI_API_KEY and OPENAI_API_KEY, both are asked and
/// the first valid answer wins. TEXT_LLM_BASE_URL and TEXT_LLM_MODEL configure the OpenAI
/// side (any compatible endpoint); GEMINI_MODEL and TEXT_LLM_REASONING_EFFORT the Gemini side.
fn language_model(http: reqwest::Client) -> Option<Arc<dyn LanguageModel>> {
    let mut models: Vec<Arc<dyn LanguageModel>> = Vec::new();
    if let Some(key) = env("GEMINI_API_KEY") {
        models.push(Arc::new(OpenAi::gemini(
            http.clone(),
            key,
            None,
            env("GEMINI_MODEL"),
            env("TEXT_LLM_REASONING_EFFORT"),
        )));
    }
    if let Some(key) = env("OPENAI_API_KEY") {
        models.push(Arc::new(OpenAi::new(http, key, env("TEXT_LLM_BASE_URL"), env("TEXT_LLM_MODEL"))));
    }
    match models.len() {
        0 => None,
        1 => models.pop(),
        _ => Some(Arc::new(FirstAnswer::new(models))),
    }
}

/// One agent model by name: a `gemini-*` model through Gemini (GEMINI_API_KEY, thinking
/// `low` unless AGENT_REASONING_EFFORT says otherwise), anything else through OpenAI
/// (OPENAI_API_KEY, reasoning `none` by default). `None` without the provider's key.
fn agent_model_named(http: reqwest::Client, model: &str, effort: Option<String>) -> Option<Arc<dyn LanguageModel>> {
    let effort = effort.or_else(|| env("AGENT_REASONING_EFFORT"));
    if model.starts_with("gemini") {
        let key = env("GEMINI_API_KEY")?;
        return Some(Arc::new(OpenAi::gemini(http, key, None, Some(model.into()), effort)));
    }
    let key = env("OPENAI_API_KEY")?;
    Some(Arc::new(OpenAi::agent(http, key, env("TEXT_LLM_BASE_URL"), Some(model.into()), effort)))
}

/// The conversation agent: AGENT_MODEL (default gemini-3.8-flash), hedged after
/// AGENT_HEDGE_MS (default 1200) by AGENT_BACKUP_MODEL (default gpt-6-luna) when that
/// provider's key is set. Run `callora eval --model <a> --model <b>` to compare models on
/// the recorded conversations before changing either.
fn agent_model(http: reqwest::Client) -> Option<Arc<dyn LanguageModel>> {
    let primary = env("AGENT_MODEL").unwrap_or_else(|| callora_providers::openai::AGENT_MODEL.into());
    let backup = env("AGENT_BACKUP_MODEL").unwrap_or_else(|| callora_providers::openai::AGENT_BACKUP_MODEL.into());
    let hedge = std::time::Duration::from_millis(env("AGENT_HEDGE_MS").and_then(|v| v.parse().ok()).unwrap_or(1200));
    let primary_model = agent_model_named(http.clone(), &primary, None)?;
    Some(match agent_model_named(http, &backup, None) {
        Some(backup_model) => Arc::new(Hedged::new(primary_model, backup_model, hedge)),
        None => {
            tracing::warn!(%backup, "no key for the agent's backup model; the agent runs unhedged");
            primary_model
        }
    })
}

/// Israel's localities and streets (`STREETS_FILE`, gzipped TSV; default
/// `data/israel-streets.tsv.gz`). Missing or unreadable, places are simply not checked.
fn load_gazetteer() -> Option<Arc<callora_core::gazetteer::Gazetteer>> {
    use std::io::Read as _;
    let path = env("STREETS_FILE").unwrap_or_else(|| "data/israel-streets.tsv.gz".into());
    let mut text = String::new();
    let read =
        std::fs::File::open(&path).and_then(|f| flate2::read::GzDecoder::new(f).read_to_string(&mut text).map(|_| ()));
    if let Err(e) = read {
        tracing::warn!(path, error = %e, "no list of Israeli streets: places will not be checked");
        return None;
    }
    let mut g = callora_core::gazetteer::Gazetteer::from_tsv(&text);
    tracing::info!(localities = g.localities(), "list of Israeli streets loaded");
    // Places that are not streets (OpenStreetMap), when shipped.
    let places = env("PLACES_FILE").unwrap_or_else(|| "data/israel-places.tsv.gz".into());
    let mut text = String::new();
    match std::fs::File::open(&places)
        .and_then(|f| flate2::read::GzDecoder::new(f).read_to_string(&mut text).map(|_| ()))
    {
        Ok(()) => {
            // Outside the macro: its fields are not evaluated when the level is off.
            let added = g.add_places(&text);
            tracing::info!(places = added, "list of places loaded");
        }
        Err(e) => tracing::warn!(path = places, error = %e, "no list of places: only streets are known"),
    }
    (!g.is_empty()).then(|| Arc::new(g))
}

/// Speech recognition: Scribe (ElevenLabs) unless STT_PROVIDER=cartesia; either needs its key.
fn speech_to_text() -> Arc<dyn SpeechToText> {
    let scribe = || {
        env("ELEVENLABS_API_KEY").map(|key| {
            Arc::new(Scribe::new(key, env("ELEVENLABS_STT_URL"), env("ELEVENLABS_STT_MODEL"))) as Arc<dyn SpeechToText>
        })
    };
    let cartesia = || {
        env("CARTESIA_API_KEY").map(|key| {
            Arc::new(Cartesia::new(key, env("CARTESIA_STT_URL"), env("CARTESIA_STT_MODEL"), env("CARTESIA_VERSION")))
                as Arc<dyn SpeechToText>
        })
    };
    let chosen = match env("STT_PROVIDER").as_deref() {
        Some("cartesia") => cartesia().or_else(scribe),
        _ => scribe().or_else(cartesia),
    };
    chosen.unwrap_or_else(|| {
        tracing::error!(
            "neither ELEVENLABS_API_KEY nor CARTESIA_API_KEY is set: calls cannot be understood and will be handed off"
        );
        Arc::new(NoStt)
    })
}

fn env_snapshot() -> HashMap<String, String> {
    std::env::vars().collect()
}

fn load_registry(dir: &Path) -> anyhow::Result<BusinessRegistry> {
    BusinessRegistry::load_dir(dir, &|k| env(k)).map_err(|e| anyhow::anyhow!("{e}"))
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .unwrap_or_default()
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let json = env("LOG_FORMAT").is_none_or(|f| f != "text");
    let builder = tracing_subscriber::fmt().with_env_filter(filter).with_target(false);
    if json {
        builder.json().flatten_event(true).init();
    } else {
        builder.init();
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve => {
            init_tracing();
            serve(&cli.businesses).await
        }
        Command::Migrate => {
            init_tracing();
            let url = env("DATABASE_URL").context("DATABASE_URL is required")?;
            let pool = callora_runtime::store::connect(&url).await?;
            callora_runtime::store::migrate(&pool).await?;
            tracing::info!("migrations applied");
            Ok(())
        }
        Command::Healthcheck => {
            let port = env("PORT").unwrap_or_else(|| "3000".into());
            let ok = reqwest::Client::new()
                .get(format!("http://127.0.0.1:{port}/health"))
                .timeout(Duration::from_secs(3))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success());
            std::process::exit(if ok { 0 } else { 1 });
        }
        Command::Config { command: ConfigCommand::Validate } => {
            let reg = load_registry(&cli.businesses)?;
            for b in reg.all() {
                println!(
                    "{}: ok ({} intents, {} pipelines, {} responses, {} library clips, {} numbers{})",
                    b.config.id,
                    b.config.intents.len(),
                    b.config.pipelines.len(),
                    b.config.responses.len(),
                    library_entries(b).len(),
                    b.phone_numbers.len(),
                    if b.handoff_number.is_some() { ", handoff desk set" } else { "" }
                );
            }
            Ok(())
        }
        Command::VoiceLibrary { command } => voice_library(&cli.businesses, command).await,
        Command::Simulate { business } => simulate(&cli.businesses, &business).await,
        Command::AgentPrompt { business } => {
            let reg = load_registry(&cli.businesses)?;
            let b = reg.by_id(&business).context("unknown business")?;
            let engine = Engine::new(b.clone(), 1);
            let request = callora_core::agent::build_request(&b, &engine.state, "…");
            println!("{}", serde_json::json!({ "system": request.system, "schema": request.schema }));
            Ok(())
        }
        Command::Eval { cases, models, reasoning, repeat, only, concurrency, json, check, min_pass, rpm } => {
            let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into());
            tracing_subscriber::fmt().with_env_filter(filter).with_target(false).init();
            run_eval(
                &cli.businesses,
                EvalArgs { cases, models, reasoning, repeat, only, concurrency, json, check, min_pass, rpm },
            )
            .await
        }
        Command::Understand { business, text } => {
            let reg = load_registry(&cli.businesses)?;
            let b = reg.by_id(&business).context("unknown business")?;
            let engine = Engine::new(b.clone(), 1);
            let (u, needs_llm) = fast_path(&b, &engine.context(), &text);
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({ "understanding": u, "needs_llm": needs_llm }))?
            );
            Ok(())
        }
    }
}

async fn voice_library(dir: &Path, command: LibraryCommand) -> anyhow::Result<()> {
    let reg = load_registry(dir)?;
    match command {
        LibraryCommand::Build { business, out, concurrency, dry_run } => {
            let businesses = match &business {
                Some(id) => vec![reg.by_id(id).context("unknown business")?],
                None => reg.all().cloned().collect(),
            };
            let mut failed = 0;
            for b in businesses {
                let entries = library_entries(&b);
                if dry_run {
                    for e in &entries {
                        println!("[{}] {:<24} {}", e.delivery, e.response_id, e.text);
                    }
                    println!("{}: {} clips", b.config.id, entries.len());
                    continue;
                }
                let api_key =
                    env("ELEVENLABS_API_KEY").context("ELEVENLABS_API_KEY is required to generate the library")?;
                let voice_id =
                    b.voice_id.clone().context("the business voice id is not set (see voice.voice_id_env)")?;
                let model = env("ELEVENLABS_LIBRARY_MODEL").unwrap_or_else(|| b.config.voice.library_model.clone());
                let synth: Arc<dyn Synthesizer> =
                    Arc::new(ElevenLabs::new(http(), api_key, env("ELEVENLABS_API_BASE_URL")));
                println!("Generating up to {} clips for `{}` with {model}...", entries.len(), b.config.id);
                let report = LibraryBuilder {
                    business: &b,
                    synthesizer: synth,
                    voice_id,
                    model,
                    root: out.clone(),
                    concurrency,
                }
                .build()
                .await?;
                println!(
                    "total {} · generated {} · reused {} · failed {}",
                    report.total,
                    report.generated,
                    report.reused,
                    report.failed.len()
                );
                for f in &report.failed {
                    println!("  failed: {f}");
                }
                failed += report.failed.len();
            }
            if failed > 0 {
                std::process::exit(1);
            }
            Ok(())
        }
        LibraryCommand::Status { dir } => {
            for b in reg.all() {
                let lib = VoiceLibrary::load(&dir, b)?;
                let wanted = library_entries(b);
                let have = wanted.iter().filter(|e| lib.get(&e.delivery, &e.text).is_some()).count();
                println!("{}: {have}/{} clips", b.config.id, wanted.len());
            }
            Ok(())
        }
    }
}

/// STT for a server started without a speech recognition key: every call is handed off.
struct NoStt;

#[async_trait::async_trait]
impl SpeechToText for NoStt {
    async fn open(&self, _language: &str, _keyterms: &[String]) -> anyhow::Result<SttSession> {
        anyhow::bail!("no speech recognition key is set")
    }
    fn name(&self) -> &'static str {
        "none"
    }
}

struct NoTelephony;

#[async_trait::async_trait]
impl Telephony for NoTelephony {
    async fn hangup(&self, call_sid: &str) -> anyhow::Result<()> {
        tracing::warn!(call_sid, "hangup requested but Twilio REST is not configured");
        Ok(())
    }
    async fn transfer(&self, call_sid: &str, to: &str, _whisper: Option<&str>) -> anyhow::Result<()> {
        tracing::warn!(call_sid, to, "transfer requested but Twilio REST is not configured");
        Ok(())
    }
}

async fn serve(dir: &Path) -> anyhow::Result<()> {
    let registry = load_registry(dir)?;
    tracing::info!(businesses = registry.len(), "business configuration loaded");
    let library_dir = PathBuf::from(env("AUDIO_LIBRARY_DIR").unwrap_or_else(|| "voice-library".into()));
    let mut libraries = HashMap::new();
    for b in registry.all() {
        libraries.insert(b.config.id.clone(), Arc::new(VoiceLibrary::load(&library_dir, b)?));
        if b.phone_numbers.is_empty() {
            tracing::warn!(business = %b.config.id, "no phone numbers configured: this business cannot be called");
        }
    }

    let client = http();
    let stt = speech_to_text();
    tracing::info!(stt = stt.name(), "speech recognition");
    let llm = language_model(client.clone());
    if llm.is_none() {
        tracing::warn!(
            "neither GEMINI_API_KEY nor OPENAI_API_KEY is set: understanding uses the deterministic fast path only"
        );
    }
    let tts: Option<Arc<dyn Synthesizer>> = env("ELEVENLABS_API_KEY").map(|key| {
        Arc::new(ElevenLabs::new(client.clone(), key, env("ELEVENLABS_API_BASE_URL"))) as Arc<dyn Synthesizer>
    });
    if tts.is_none() {
        tracing::warn!("ELEVENLABS_API_KEY is not set: only pre-generated audio can be played");
    }
    let twilio_token = env("TWILIO_AUTH_TOKEN");
    let telephony: Arc<dyn Telephony> = match (env("TWILIO_ACCOUNT_SID"), twilio_token.clone()) {
        (Some(sid), Some(token)) => Arc::new(TwilioRest::new(client.clone(), sid, token, None)),
        _ => {
            tracing::warn!("TWILIO_ACCOUNT_SID/TWILIO_AUTH_TOKEN not set: hangups and transfers are disabled");
            Arc::new(NoTelephony)
        }
    };

    let db = match env("DATABASE_URL") {
        Some(url) => match callora_runtime::store::connect(&url).await {
            Ok(pool) => {
                if let Err(e) = callora_runtime::store::migrate(&pool).await {
                    tracing::error!(error = %e, "database migrations failed; call history disabled");
                    None
                } else {
                    Some(pool)
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "database unavailable; call history disabled");
                None
            }
        },
        None => None,
    };
    let store: Arc<dyn CallStore> = match &db {
        Some(pool) => Arc::new(callora_runtime::store::PgStore::spawn(pool.clone())),
        None => Arc::new(NullStore),
    };
    if let Some(pool) = db.clone() {
        let days: u32 = env("TRANSCRIPT_RETENTION_DAYS").and_then(|d| d.parse().ok()).unwrap_or(30);
        tokio::spawn(async move {
            loop {
                match callora_runtime::store::prune_transcripts(&pool, days).await {
                    Ok(n) if n > 0 => tracing::info!(deleted = n, days, "old transcripts pruned"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "transcript pruning failed"),
                }
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        });
    }

    let gazetteer = load_gazetteer();
    let agent = agent_model(client.clone());
    if agent.is_none() && registry.all().any(|b| b.config.agent.is_some()) {
        tracing::warn!("no key for AGENT_MODEL's provider: businesses with an agent fall back to the rules");
    }
    // The second hearing uses the same ElevenLabs key as the stream. Off unless
    // SECOND_HEARING=1: with a city's streets as hints it made names up on live calls
    // ("44, 45" heard "בן זכריה ארבעים וחמש"), so it waits for measured results.
    let second_hearing =
        env("ELEVENLABS_API_KEY").filter(|_| env("SECOND_HEARING").as_deref() == Some("1")).map(|key| {
            Arc::new(callora_providers::scribe_batch::ScribeBatch::new(
                http(),
                key,
                None,
                env("ELEVENLABS_BATCH_STT_MODEL"),
            )) as Arc<dyn callora_runtime::ports::Transcriber>
        });
    let services = Services {
        stt,
        llm,
        agent,
        second_hearing,
        gazetteer,
        tts,
        tts_cache: TtsCache::new(env("TTS_CACHE_ENTRIES").and_then(|v| v.parse().ok()).unwrap_or(2000)),
        actions: Arc::new(ConfiguredActions::new(client.clone(), env_snapshot())),
        telephony,
        store,
        whisper: Arc::new(NoWhisper),
        metrics: Arc::new(Metrics::default()),
    };
    let mut session = SessionConfig { dynamic_model: env("ELEVENLABS_DYNAMIC_MODEL"), ..SessionConfig::default() };
    if let Some(ms) = env("VAD_ENDPOINT_MS").and_then(|v| v.parse().ok()) {
        session.vad.endpoint_ms = ms;
    }
    session.agent_speculate = env("AGENT_SPECULATE").is_some_and(|v| v == "true" || v == "1");
    session.sample_audio_from = env("AUDIO_SAMPLE_NUMBERS").map(|l| parse_allow_list(&l)).unwrap_or_default();
    if let Some(ms) = env("VAD_TRIGGER_MS").and_then(|v| v.parse().ok()) {
        session.vad.trigger_ms = ms;
    }

    let public_base_url = env("PUBLIC_BASE_URL").unwrap_or_default().trim_end_matches('/').to_string();
    let skip_signature_validation = env("TWILIO_SKIP_SIGNATURE_VALIDATION").is_some_and(|v| v == "true");
    if twilio_token.is_none() && !skip_signature_validation {
        anyhow::bail!("TWILIO_AUTH_TOKEN is required (it validates every Twilio webhook)");
    }
    if public_base_url.is_empty() {
        tracing::warn!(
            "PUBLIC_BASE_URL is not set: Twilio signatures cannot validate and media streams cannot connect"
        );
    }
    let mut stream_secrets: Vec<String> = Vec::new();
    stream_secrets.extend(env("STREAM_TOKEN_SECRET"));
    stream_secrets.extend(twilio_token.clone());
    if stream_secrets.is_empty() {
        stream_secrets.push(hex_random());
    }
    let settings = ServerSettings {
        public_base_url,
        twilio_auth_token: twilio_token.unwrap_or_default(),
        stream_secrets,
        allow_list: env("ALLOW_LIST").map(|l| parse_allow_list(&l)).unwrap_or_default(),
        admin_api_key: env("ADMIN_API_KEY").filter(|k| k.len() >= 8),
        skip_signature_validation,
        prices: callora_runtime::pricing::parse_prices(&env("AGENT_PRICES").unwrap_or_default()),
    };
    let state = AppState::new(registry, libraries, services, session, settings, db);
    let app = router(state);
    let addr =
        format!("{}:{}", env("HOST").unwrap_or_else(|| "0.0.0.0".into()), env("PORT").unwrap_or_else(|| "3000".into()));
    let listener = tokio::net::TcpListener::bind(&addr).await.with_context(|| format!("cannot bind {addr}"))?;
    tracing::info!(%addr, "callora listening");
    axum::serve(listener, app).with_graceful_shutdown(shutdown()).await?;
    Ok(())
}

fn hex_random() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{nanos:x}{:p}", &nanos)
}

/// Accepts the same loose formats the legacy deployment did: commas, semicolons or
/// newlines, optional brackets and quotes, and spaces/dashes inside numbers.
fn parse_allow_list(raw: &str) -> Vec<String> {
    raw.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split([',', ';', '\n', '\r'])
        .map(|e| e.trim().trim_matches(['"', '\'']).chars().filter(|c| !" -().".contains(*c)).collect::<String>())
        .filter(|e| !e.is_empty())
        .collect()
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = term => {},
    }
    tracing::info!("shutting down");
}

struct EvalArgs {
    cases: PathBuf,
    models: Vec<String>,
    reasoning: Option<String>,
    repeat: usize,
    only: Option<String>,
    concurrency: usize,
    json: Option<PathBuf>,
    check: bool,
    min_pass: Option<f64>,
    rpm: Option<u32>,
}

async fn run_eval(dir: &Path, args: EvalArgs) -> anyhow::Result<()> {
    use futures::StreamExt as _;

    // As in production, a human desk takes handoffs; an unset number would turn every
    // transfer into "no one is available".
    let lookup = |k: &str| env(k).or_else(|| k.ends_with("HANDOFF_NUMBER").then(|| "+972500000001".to_string()));
    let registry = Arc::new(BusinessRegistry::load_dir(dir, &lookup).map_err(|e| anyhow::anyhow!("{e}"))?);
    let mut cases = eval::load_cases(&args.cases)?;
    if let Some(only) = &args.only {
        cases.retain(|c| c.id.contains(only.as_str()));
    }
    let problems: Vec<String> = cases.iter().flat_map(|c| eval::check_case(c, &registry)).collect();
    if !problems.is_empty() {
        for p in &problems {
            eprintln!("{p}");
        }
        anyhow::bail!("{} problem(s) in the cases", problems.len());
    }
    println!("{} cases, well formed.", cases.len());
    if args.check {
        return Ok(());
    }
    anyhow::ensure!(!cases.is_empty(), "no case to run");

    let client = http();
    let mut models: Vec<(String, Arc<dyn LanguageModel>)> = Vec::new();
    if args.models.is_empty() {
        let name = format!(
            "production ({} hedged by {})",
            env("AGENT_MODEL").unwrap_or_else(|| callora_providers::openai::AGENT_MODEL.into()),
            env("AGENT_BACKUP_MODEL").unwrap_or_else(|| callora_providers::openai::AGENT_BACKUP_MODEL.into())
        );
        models.push((name, agent_model(client.clone()).context("the agent model's API key is not set")?));
    }
    for m in &args.models {
        let model = agent_model_named(client.clone(), m, args.reasoning.clone())
            .with_context(|| format!("no API key for {m} (GEMINI_API_KEY or OPENAI_API_KEY)"))?;
        let label = match args.reasoning.clone().or_else(|| env("AGENT_REASONING_EFFORT")) {
            Some(e) if callora_providers::openai::is_reasoning_model(m) => format!("{m} (reasoning {e})"),
            _ => m.clone(),
        };
        models.push((label, model));
    }
    let runner = Arc::new(eval::Runner {
        registry,
        gazetteer: load_gazetteer(),
        // Mock backends only: an eval never books a real ride.
        actions: Arc::new(ConfiguredActions::new(client, HashMap::new())),
        pacer: args
            .rpm
            .filter(|r| *r > 0)
            .map(|rpm| Arc::new(eval::Pacer::new(Duration::from_millis(60_000 / u64::from(rpm))))),
    });
    let prices =
        callora_runtime::pricing::parse_prices(&env("EVAL_PRICES").or_else(|| env("AGENT_PRICES")).unwrap_or_default());
    let mut reports = Vec::new();
    for (label, model) in models {
        println!("Running {} cases × {} with {label}...", cases.len(), args.repeat);
        let jobs = cases.iter().flat_map(|c| (0..args.repeat as u64).map(move |i| (c, i)));
        let runs: Vec<eval::CaseRun> = futures::stream::iter(jobs)
            .map(|(c, i)| {
                let runner = runner.clone();
                let model = model.clone();
                async move { runner.run_case(c, model.as_ref(), 1000 + i).await }
            })
            .buffer_unordered(args.concurrency.max(1))
            .collect()
            .await;
        reports.push(eval::summarize(&label, runs, &prices));
    }
    print!("{}", eval::render(&reports));
    if let Some(path) = &args.json {
        std::fs::write(path, serde_json::to_string_pretty(&reports)?)?;
        println!(
            "
Full report: {}",
            path.display()
        );
    }
    if let Some(min) = args.min_pass {
        let below: Vec<&str> = reports.iter().filter(|r| r.pass_rate < min).map(|r| r.model.as_str()).collect();
        anyhow::ensure!(below.is_empty(), "pass rate below {min} for {}", below.join(", "));
    }
    Ok(())
}

async fn simulate(dir: &Path, business: &str) -> anyhow::Result<()> {
    let reg = load_registry(dir)?;
    let b: Arc<Business> = reg.by_id(business).context("unknown business")?;
    let llm = language_model(http());
    let actions = ConfiguredActions::new(http(), env_snapshot());
    let info = CallInfo {
        call_id: Default::default(),
        call_sid: "SIMULATED".into(),
        business_id: b.config.id.clone(),
        from: Some("+972500000000".into()),
        to: String::new(),
    };
    let mut engine = Engine::new(b.clone(), 42);
    engine.set_gazetteer(load_gazetteer());
    engine.set_caller_phone(info.from.clone());
    // The same agent as a live call, when the business has one.
    let agent = if b.config.agent.is_some() { agent_model(http()) } else { None };
    println!(
        "Simulating `{}` ({}). Type what the caller says; empty line = silence; Ctrl-D to quit.",
        b.config.id, b.config.name
    );
    println!(
        "{}\n",
        match (&agent, &llm) {
            (Some(_), _) => "Agent: on",
            (None, Some(_)) => "LLM: on",
            (None, None) => "LLM: off (fast path only)",
        }
    );
    let mut pending = engine.start();
    let stdin = std::io::stdin();
    loop {
        // Run directives, feeding action results back in, until the engine waits for the caller.
        while !pending.is_empty() {
            let mut next = Vec::new();
            for d in std::mem::take(&mut pending) {
                match d {
                    Directive::Speak { plan, filler } => {
                        for s in &plan.segments {
                            println!(
                                "  agent{} [{:?}/{}] {}",
                                if filler { " (filler)" } else { "" },
                                s.origin,
                                s.delivery,
                                s.text
                            );
                        }
                    }
                    Directive::RunAction { run_id, action, input } => {
                        println!("  · action {action} {}", serde_json::to_string(&input["slots"])?);
                        let result = actions.run(&b, &action, input, &info).await;
                        println!("  · result {result:?}");
                        next.extend(engine.on_action_result(run_id, result));
                    }
                    Directive::Handoff { summary } => println!("  · HANDOFF ({}) — {}", summary.reason, summary.text),
                    Directive::Hangup => {
                        println!("  · HANGUP");
                        for card in callora_core::orders::order_cards(&b, &engine.state) {
                            println!("  · ORDER {}", card["summary"].as_str().unwrap_or(""));
                        }
                        return Ok(());
                    }
                }
            }
            pending = next;
        }
        print!("caller> ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            return Ok(());
        }
        let text = line.trim();
        if text.is_empty() {
            pending = engine.on_silence();
            continue;
        }
        if let Some(model) = &agent {
            let request = callora_core::agent::build_request(&b, &engine.state, text);
            let started = std::time::Instant::now();
            match model.extract(&request).await {
                Ok(reply) => {
                    println!("  · agent ({} ms) {reply}", started.elapsed().as_millis());
                    let turn = callora_core::agent::parse(&b, &reply);
                    pending = engine.on_agent_turn(text, turn, "");
                }
                Err(e) => {
                    println!("  · agent failed: {e:#}");
                    pending = engine.on_silence();
                }
            }
            let slots = engine.state.run.as_ref().map(|r| {
                r.slots.iter().map(|(k, v)| format!("{k}={}", v.value.spoken())).collect::<Vec<_>>().join(", ")
            });
            println!(
                "  · form={:?} step={:?} slots=[{}] notes={:?}",
                engine.state.address_form,
                engine.state.run.as_ref().map(|r| &r.step),
                slots.unwrap_or_default(),
                engine.state.agent_notes
            );
            continue;
        }
        let (fast, needs_llm) = fast_path(&b, &engine.context(), text);
        let u = match (&llm, needs_llm) {
            (Some(model), true) => {
                let req = callora_core::llm::build_request(&b, &engine.context(), &engine.state, text);
                match model.extract(&req).await {
                    Ok(reply) => merge(fast, callora_core::llm::parse_response(&b, &engine.context(), text, &reply)),
                    Err(e) => {
                        println!("  · llm failed: {e:#}");
                        fast
                    }
                }
            }
            _ => fast,
        };
        println!(
            "  · understood: meta={:?} intent={:?} affirm={:?} slots={}",
            u.meta,
            u.intent.as_ref().map(|i| &i.id),
            u.affirm,
            u.slots.iter().map(|s| format!("{}={}", s.slot, s.value.spoken())).collect::<Vec<_>>().join(", ")
        );
        pending = engine.on_utterance(u);
    }
}

#[cfg(test)]
mod streets {
    use callora_core::gazetteer::Lookup;

    /// The real list, as shipped in the image.
    #[test]
    fn the_shipped_list_resolves_real_places() {
        std::env::set_var("STREETS_FILE", concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/israel-streets.tsv.gz"));
        std::env::set_var("PLACES_FILE", concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/israel-places.tsv.gz"));
        let g = super::load_gazetteer().expect("the list loads");
        assert!(g.localities() > 1200, "{}", g.localities());
        let spoken = |t: &str| match g.resolve(t) {
            Lookup::Found(a) => a.spoken(),
            other => panic!("{t}: {other:?}"),
        };
        assert_eq!(spoken("זבוטינסקי 5 רמת גן"), "ז'בוטינסקי 5, רמת גן");
        assert_eq!(spoken("מאלעד"), "אלעד");
        assert_eq!(spoken("דיזנגוף 50 תל אביב"), "דיזנגוף 50, תל אביב");
        assert_eq!(spoken("לבאר שבע"), "באר שבע");
        // "לאלעד" heard as "לעדו", written "עדי" (a moshav): the street names the city.
        assert_eq!(spoken("בן זכאי 45, עדי"), "רבן יוחנן בן זכאי 45, אלעד");
        // "ביתר" is ביתר עילית, not מיתר (a live call booked "רימון 16, מיתר").
        assert!(spoken("הרמב\"ן 16, ביתר").ends_with("ביתר עילית"), "{}", spoken("הרמב\"ן 16, ביתר"));
        // Real streets of the city said stay there.
        assert_eq!(spoken("אחוזה 12 רעננה"), "אחוזה 12, רעננה");
        assert_eq!(spoken("הרצל 10 רחובות"), "הרצל 10, רחובות");
        // Places that are not streets, from the OpenStreetMap list.
        match g.resolve("בנייני האומה, ירושלים") {
            Lookup::Found(a) => {
                assert_eq!(a.spoken(), "בנייני האומה, ירושלים");
                assert!(a.place.is_some_and(|p| p.point.starts_with("31.78")), "with its point");
            }
            other => panic!("{other:?}"),
        }
        match g.resolve("מיל״ד") {
            Lookup::NoCity { closest } => assert!(closest.iter().any(|c| c == "אלעד"), "{closest:?}"),
            other => panic!("{other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_allow_list;

    #[test]
    fn allow_list_formats() {
        assert_eq!(parse_allow_list("[\"+972 50-123-4567\"; +972509998888]"), vec!["+972501234567", "+972509998888"]);
    }
}

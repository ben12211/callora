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
    deepgram::Deepgram,
    elevenlabs::ElevenLabs,
    openai::OpenAi,
    race::{Fallback, FirstAnswer, Hedged},
    scribe::Scribe,
    twilio_rest::TwilioRest,
};
use callora_runtime::actions::ConfiguredActions;
use callora_runtime::agent_model::{
    AgentControl, AgentModelSettings, AssembledModel, NamedModel, Providers, SwitchableModel, NO_BACKUP,
};
use callora_runtime::metrics::Metrics;
use callora_runtime::ports::{
    ActionRunner, CallInfo, CallStore, LanguageModel, NoWhisper, NullStore, SpeechToText, SttSession, Telephony,
};
use callora_runtime::server::{router, AppState, ServerSettings};
use callora_runtime::session::{Services, SessionConfig};

mod eval;
mod noise_probe;

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
    /// Recorded caller audio in noise through the VAD and the configured speech recognizer,
    /// scored against what was said (see noise_probe.rs).
    NoiseProbe {
        file: PathBuf,
        /// The VAD of production (`noise`: following the background, RNNoise's voice
        /// probability) or the energy alone (`energy`).
        #[arg(long, default_value = "noise")]
        vad: String,
        /// Send RNNoise's cleaned audio to the recognizer instead of the call's.
        #[arg(long)]
        clean: bool,
        /// Print every clip that lost words or got words from nothing.
        #[arg(long)]
        show: bool,
    },
    /// Stored caller utterances through the configured speech recognizer, with its latency.
    SttProbe {
        /// JSON lines with `heard` and `audio` (base64 μ-law 8 kHz).
        file: PathBuf,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// The hints a call gives while this city's street is asked: the city and its streets.
        #[arg(long)]
        city: Option<String>,
        /// The business's words as hints (what a call gives between street questions).
        #[arg(long)]
        business_words: bool,
        /// Also the towns of the taxi business's service area, as live calls are given them.
        #[arg(long)]
        area: bool,
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
    /// Synthesize the same sentences with different voice settings, to listen and compare
    /// (`stability`, `style`, speaker boost), as a caller would hear them (tempo and gain
    /// applied). Writes WAV files and an `index.html` of players. Calls ElevenLabs.
    Ab {
        #[arg(long)]
        business: String,
        #[arg(long, default_value = "voice-ab")]
        out: PathBuf,
        /// Model to synthesize with; the business's dynamic model when omitted.
        #[arg(long)]
        model: Option<String>,
        /// `name=stability[,style[,boost]]`, repeatable. Without any: `current` (the
        /// business's settings), `natural` (stability 0.5) and `expressive` (0.5, style 0.25).
        #[arg(long = "preset")]
        presets: Vec<String>,
        /// Sentences per preset, taken from the business's responses.
        #[arg(long, default_value_t = 6)]
        sentences: usize,
        /// Gain applied to what is written, as on a call.
        #[arg(long)]
        gain_db: Option<f32>,
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
    // OpenAI's priority processing unless AGENT_SERVICE_TIER says otherwise ("default").
    let tier = env("AGENT_SERVICE_TIER").or_else(|| Some("priority".into()));
    Some(Arc::new(
        OpenAi::agent(http, key, env("TEXT_LLM_BASE_URL"), Some(model.into()), effort).with_service_tier(tier),
    ))
}

/// What the environment says about the agent's model (AGENT_MODEL, AGENT_BACKUP_MODEL,
/// AGENT_REASONING_EFFORT), with the defaults for what it leaves out.
fn agent_model_env() -> AgentModelSettings {
    AgentModelSettings {
        primary: env("AGENT_MODEL").unwrap_or_else(|| callora_providers::openai::AGENT_MODEL.into()),
        backup: env("AGENT_BACKUP_MODEL").unwrap_or_else(|| callora_providers::openai::AGENT_BACKUP_MODEL.into()),
        effort: env("AGENT_REASONING_EFFORT"),
    }
}

/// The conversation agent: the primary model (default gemini-3.8-flash), hedged after
/// AGENT_HEDGE_MS (default 1100) by the backup (default gpt-6-luna, `none` for no hedge) when
/// that provider's key is set; when both fail, AGENT_FALLBACK_MODEL, by default a model of the
/// other provider (an outage of one provider must not end the calls). Run `callora eval
/// --model <a> --model <b>` to compare models on the recorded conversations before changing
/// any.
fn agent_model_with(http: reqwest::Client, models: &AgentModelSettings) -> Option<Arc<dyn LanguageModel>> {
    let (primary, backup) = (models.primary.as_str(), models.backup.as_str());
    let hedge = std::time::Duration::from_millis(env("AGENT_HEDGE_MS").and_then(|v| v.parse().ok()).unwrap_or(1100));
    let primary_model = agent_model_named(http.clone(), primary, models.effort.clone())?;
    let backup_model = (backup != NO_BACKUP).then(|| agent_model_named(http.clone(), backup, models.effort.clone()));
    let agent: Arc<dyn LanguageModel> = match backup_model {
        Some(Some(backup_model)) => Arc::new(Hedged::new(primary_model, backup_model, hedge)),
        Some(None) => {
            tracing::warn!(%backup, "no key for the agent's backup model; the agent runs unhedged");
            primary_model
        }
        None => primary_model,
    };
    let on_gemini = primary.starts_with("gemini") && (backup == NO_BACKUP || backup.starts_with("gemini"));
    let on_openai = !primary.starts_with("gemini") && (backup == NO_BACKUP || !backup.starts_with("gemini"));
    let fallback = env("AGENT_FALLBACK_MODEL").or_else(|| {
        if on_openai {
            Some("gemini-3.8-flash".into())
        } else if on_gemini {
            Some(callora_providers::openai::AGENT_BACKUP_MODEL.into())
        } else {
            None
        }
    });
    Some(match fallback.as_deref().filter(|f| *f != "none").and_then(|f| agent_model_named(http, f, None)) {
        Some(then) => {
            tracing::info!(fallback = ?fallback, "the agent falls back to another provider when its models fail");
            Arc::new(Fallback::new(agent, then))
        }
        None => agent,
    })
}

/// The agent as the environment configures it.
fn agent_model(http: reqwest::Client) -> Option<Arc<dyn LanguageModel>> {
    agent_model_with(http, &agent_model_env())
}

/// The agent's model, switchable from the settings page: the model saved there when it can be
/// built, else the environment's. `None` without a key for any of them.
fn switchable_agent(
    http: reqwest::Client,
    settings: &callora_runtime::settings::SettingsStore,
) -> Option<Arc<AgentControl>> {
    let defaults = agent_model_env();
    let saved = settings.agent_model();
    let built = saved
        .as_ref()
        .and_then(|m| agent_model_with(http.clone(), m).map(|a| (a, m.clone())))
        .or_else(|| agent_model_with(http.clone(), &defaults).map(|a| (a, defaults.clone())));
    let (agent, active) = built?;
    if saved.as_ref().is_some_and(|m| *m != active) {
        tracing::warn!(
            "the agent's model chosen on the settings page cannot be built (no key?); using the environment's"
        );
    }
    let named: NamedModel = {
        let http = http.clone();
        Arc::new(move |model: &str, effort: Option<String>| agent_model_named(http.clone(), model, effort))
    };
    let assemble: AssembledModel = Arc::new(move |models: &AgentModelSettings| agent_model_with(http.clone(), models));
    let providers = Providers { gemini: env("GEMINI_API_KEY").is_some(), openai: env("OPENAI_API_KEY").is_some() };
    let catalog = [
        callora_providers::openai::AGENT_MODEL,
        callora_providers::openai::OPENAI_AGENT_MODEL,
        callora_providers::openai::AGENT_BACKUP_MODEL,
    ]
    .map(String::from)
    .to_vec();
    Some(Arc::new(AgentControl::new(
        Arc::new(SwitchableModel::new(agent)),
        active,
        defaults,
        named,
        assemble,
        providers,
        catalog,
    )))
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

/// Speech recognition: Deepgram Nova-3 unless STT_PROVIDER names OpenAI (`gpt-transcribe`),
/// Scribe (ElevenLabs) or Cartesia. Each needs its key; without the chosen one's, the next
/// one with a key hears. OpenAI is backed by the next one if its session cannot open.
fn speech_to_text() -> Arc<dyn SpeechToText> {
    let openai = || {
        env("OPENAI_API_KEY").map(|key| {
            Arc::new(
                callora_providers::openai_stt::OpenAiStt::new(
                    key,
                    env("OPENAI_STT_URL"),
                    env("OPENAI_STT_MODEL"),
                    env("OPENAI_STT_PROMPT"),
                )
                .with_hints(env("OPENAI_STT_HINTS").as_deref() == Some("1"))
                .with_noise_reduction(env("OPENAI_STT_NOISE_REDUCTION"))
                .with_logprobs(env("OPENAI_STT_LOGPROBS").as_deref() == Some("1")),
            ) as Arc<dyn SpeechToText>
        })
    };
    let deepgram = || {
        env("DEEPGRAM_API_KEY").map(|key| {
            Arc::new(Deepgram::new(key, env("DEEPGRAM_STT_URL"), env("DEEPGRAM_STT_MODEL"))) as Arc<dyn SpeechToText>
        })
    };
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
        Some("openai") => match (openai(), deepgram().or_else(scribe).or_else(cartesia)) {
            (Some(primary), Some(backup)) => {
                Some(Arc::new(callora_providers::stt_failover::SttFailover::new(primary, backup))
                    as Arc<dyn SpeechToText>)
            }
            (primary, backup) => primary.or(backup),
        },
        Some("cartesia") => cartesia().or_else(deepgram).or_else(scribe),
        Some("scribe") => scribe().or_else(deepgram).or_else(cartesia),
        _ => deepgram().or_else(scribe).or_else(cartesia),
    };
    chosen.unwrap_or_else(|| {
        tracing::error!(
            "none of DEEPGRAM_API_KEY, ELEVENLABS_API_KEY or CARTESIA_API_KEY is set: calls cannot be understood and will be handed off"
        );
        Arc::new(NoStt)
    })
}

/// The built dashboard: `WEB_DIR`, else `web/dist` when it has been built.
fn web_dir() -> Option<std::path::PathBuf> {
    let dir = std::path::PathBuf::from(env("WEB_DIR").unwrap_or_else(|| "web/dist".into()));
    if dir.join("index.html").is_file() {
        tracing::info!(dir = %dir.display(), "dashboard");
        Some(dir)
    } else {
        tracing::warn!(dir = %dir.display(), "no dashboard build: the site is not served");
        None
    }
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
        // A backstop: nothing a call waits on may hang it (each caller has a shorter one).
        .timeout(Duration::from_secs(30))
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
        Command::NoiseProbe { file, vad, clean, show } => {
            init_tracing();
            let base = callora_audio::vad::VadConfig::default();
            let (cfg, voice) = if vad == "energy" { (base, false) } else { (base.for_noise(), true) };
            noise_probe::run(speech_to_text(), &file, cfg, voice, clean, show).await
        }
        Command::SttProbe { file, limit, city, business_words, area } => {
            init_tracing();
            let mut terms = Vec::new();
            let towns = if area {
                load_registry(&cli.businesses)?.by_id("taxi").context("no taxi business")?.config.service_area.clone()
            } else {
                Vec::new()
            };
            if let Some(city) = &city {
                let g = load_gazetteer().context("the streets list is needed for --city")?;
                terms.push(city.clone());
                terms.extend(towns.iter().cloned());
                terms.extend(g.street_keyterms(city, 38));
            } else {
                terms.extend(towns.iter().cloned());
            }
            if business_words {
                let reg = load_registry(&cli.businesses)?;
                terms.extend(reg.by_id("taxi").context("no taxi business")?.stt_keyterms());
            }
            let mut seen = std::collections::HashSet::new();
            terms.retain(|t| seen.insert(t.clone()));
            stt_probe(&file, limit, &terms).await
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

/// The voice presets to compare: `name=stability[,style[,boost]]`, applied over the
/// business's settings.
fn parse_presets(
    specs: &[String],
    base: callora_core::config::VoiceSettings,
) -> anyhow::Result<Vec<(String, callora_core::config::VoiceSettings)>> {
    if specs.is_empty() {
        let natural = callora_core::config::VoiceSettings { stability: 0.5, ..base };
        return Ok(vec![
            ("current".into(), base),
            ("natural".into(), natural),
            ("expressive".into(), callora_core::config::VoiceSettings { style: 0.25, ..natural }),
        ]);
    }
    specs
        .iter()
        .map(|spec| {
            let (name, values) = spec.split_once('=').context("a preset is `name=stability[,style[,boost]]`")?;
            let mut it = values.split(',');
            let mut settings = base;
            settings.stability = it.next().context("stability")?.trim().parse().context("stability is a number")?;
            if let Some(v) = it.next() {
                settings.style = v.trim().parse().context("style is a number")?;
            }
            if let Some(v) = it.next() {
                settings.speaker_boost = v.trim().parse().context("boost is true or false")?;
            }
            Ok((name.trim().to_string(), settings))
        })
        .collect()
}

/// `callora voice-library ab`: the same sentences in each preset, as a caller hears them.
async fn voice_ab(
    b: &Business,
    out: &Path,
    model: Option<String>,
    specs: &[String],
    sentences: usize,
    gain_db: Option<f32>,
) -> anyhow::Result<()> {
    let api_key = env("ELEVENLABS_API_KEY").context("ELEVENLABS_API_KEY is required")?;
    let voice_id = b.voice_id.clone().context("the business voice id is not set (see voice.voice_id_env)")?;
    let model = model.unwrap_or_else(|| b.config.voice.dynamic_model.clone());
    let presets = parse_presets(specs, b.config.voice.settings)?;
    let gain = gain_db.unwrap_or(SessionConfig::default().tts_gain_db);
    let synth = ElevenLabs::new(http(), api_key, env("ELEVENLABS_API_BASE_URL"));

    // Static sentences from the business, shortest first mixed with the longest, so both a
    // quick acknowledgement and a full question are heard.
    let mut texts: Vec<(String, String)> =
        library_entries(b).into_iter().filter(|e| e.delivery == "normal").map(|e| (e.response_id, e.text)).collect();
    texts.sort_by_key(|(_, t)| t.chars().count());
    texts.dedup_by(|a, c| a.1 == c.1);
    let step = (texts.len() / sentences.max(1)).max(1);
    let picked: Vec<(String, String)> = texts.into_iter().step_by(step).take(sentences).collect();

    std::fs::create_dir_all(out)?;
    let mut html = String::from(
        "<!doctype html><meta charset=utf-8><title>voice A/B</title><body dir=rtl style=\"font-family:sans-serif\">",
    );
    html.push_str(&format!("<h2>{} · {model} · gain {gain} dB</h2>", b.config.id));
    for (name, settings) in &presets {
        let effective = callora_providers::elevenlabs::effective_stability(&model, settings.stability);
        println!(
            "{name}: stability {} (sent {effective}) style {} boost {}",
            settings.stability, settings.style, settings.speaker_boost
        );
        html.push_str(&format!(
            "<h3>{name} — stability {} (sent {effective}), style {}, boost {}</h3>",
            settings.stability, settings.style, settings.speaker_boost
        ));
        for (i, (response_id, text)) in picked.iter().enumerate() {
            let spoken = callora_audio::library::with_tone(
                b,
                response_id,
                callora_core::speech::prepare_for_tts(text, &b.config.language, &b.pronouncer),
            );
            let request = callora_audio::tts::TtsRequest {
                text: spoken,
                voice_id: voice_id.clone(),
                model: model.clone(),
                settings: *settings,
                language: b.config.language.clone(),
                previous_text: None,
            };
            let stream = synth.synthesize(request).await?;
            let chunks: Vec<bytes::Bytes> = futures::TryStreamExt::try_collect(stream).await?;
            let mut audio = chunks.concat();
            if b.config.voice.tempo != 1.0 {
                audio = callora_audio::tempo::stretch(&audio, b.config.voice.tempo);
            }
            callora_audio::mulaw::Limiter::new(SessionConfig::default().limiter_ceiling_dbfs).process(&mut audio, gain);
            let file = format!("{name}-{i}.wav");
            std::fs::write(out.join(&file), callora_audio::mulaw::to_wav(&audio))?;
            html.push_str(&format!("<p>{text}<br><audio controls src=\"{file}\"></audio></p>"));
        }
    }
    std::fs::write(out.join("index.html"), html)?;
    println!("written to {} (open index.html)", out.join("index.html").display());
    Ok(())
}

/// A number from the environment; unset or unparsable leaves the default.
fn env_num<T: std::str::FromStr>(name: &str) -> Option<T> {
    env(name).and_then(|v| v.parse().ok())
}

/// The call-voice settings that can be tuned without a rebuild (see docs/VOICE_TUNING.md).
/// Every one has a default in `SessionConfig`; each setting below is only read when set.
fn apply_voice_env(session: &mut SessionConfig) {
    macro_rules! set {
        ($name:literal, $field:expr) => {
            if let Some(v) = env_num($name) {
                $field = v;
            }
        };
    }
    // Noisy places: the VAD follows the background and asks RNNoise whether a sound is a
    // voice. On unless `VAD_NOISE=off`.
    if !env("VAD_NOISE").is_some_and(|v| v == "off" || v == "false" || v == "0") {
        session.vad = session.vad.for_noise();
    }
    set!("VAD_NOISE_FLOOR_RATIO", session.vad.noise_floor_ratio);
    set!("VAD_VOICE_HOLD", session.vad.voice_hold);
    if let Some(start) = env_num::<f32>("VAD_VOICE_START") {
        session.vad.voice_start = Some(start);
    }
    // Loudness.
    set!("VAD_THRESHOLD_RMS", session.vad.threshold_rms);
    set!("TTS_GAIN_DB", session.tts_gain_db);
    set!("TTS_GAIN_MAX_DB", session.tts_gain_max_db);
    set!("TTS_LIMITER_CEILING_DBFS", session.limiter_ceiling_dbfs);
    // Barge-in.
    set!("BARGE_CONFIRM_MS", session.barge.confirm_ms);
    set!("BARGE_CONFIRM_GREETING_MS", session.barge.confirm_greeting_ms);
    set!("BARGE_CONFIRM_READBACK_MS", session.barge.confirm_read_back_ms);
    set!("BARGE_MIN_VOICED_MS", session.barge.min_voiced_ms);
    set!("BARGE_MIN_WORDS", session.barge.min_words);
    set!("BARGE_SINGLE_WORD_MS", session.barge.single_word_ms);
    set!("BARGE_STRONG_MS", session.barge.strong_ms);
    set!("BARGE_STRONG_RMS_RATIO", session.barge.strong_rms_ratio);
    set!("BARGE_STRONG_MIN_RMS", session.barge.strong_min_rms);
    set!("BARGE_FINAL_MIN_VOICED_MS", session.barge.final_min_voiced_ms);
    // `BARGE_LEGACY=true`: words stop the agent the moment they are heard, as before.
    if env("BARGE_LEGACY").is_some_and(|v| v == "true" || v == "1") {
        let confirm = session.barge;
        session.barge = callora_runtime::barge::BargeConfig {
            confirm_ms: confirm.confirm_ms,
            confirm_greeting_ms: confirm.confirm_greeting_ms,
            confirm_read_back_ms: confirm.confirm_read_back_ms,
            ..callora_runtime::barge::BargeConfig::legacy()
        };
    }
    set!("SENTENCE_END_PROTECT_MS", session.sentence_end_protect_ms);
    // Endpointing: equal to VAD_ENDPOINT_MS turns the adaptation off.
    set!("VAD_ENDPOINT_SHORT_MS", session.endpoint_short_ms);
    set!("VAD_ENDPOINT_LONG_MS", session.endpoint_long_ms);
    // Playout seams.
    set!("TTS_START_BUFFER_MS", session.tts_start_buffer_ms);
    set!("TTS_CONTINUATION_BUFFER_MS", session.tts_continuation_buffer_ms);
    set!("TTS_REBUFFER_MS", session.tts_rebuffer_ms);
    if env("AUDIO_TRIM_SILENCE").is_some_and(|v| v == "false" || v == "0") {
        session.trim_silence = None;
    } else if let Some(t) = session.trim_silence.as_mut() {
        set!("AUDIO_TRIM_THRESHOLD_RMS", t.threshold_rms);
        set!("AUDIO_JOIN_PAD_MS", t.pad_ms);
        set!("AUDIO_TRIM_TAIL_PAD_MS", t.tail_pad_ms);
    }
    set!("AUDIO_GAP_WARN_MS", session.audio_gap_warn_ms);
    set!("AUDIO_GAP_IGNORE_MS", session.audio_gap_ignore_ms);
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
        LibraryCommand::Ab { business, out, model, presets, sentences, gain_db } => {
            let b = reg.by_id(&business).context("unknown business")?;
            voice_ab(&b, &out, model, &presets, sentences, gain_db).await
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

/// `callora stt-probe`: stored caller utterances (the JSON lines the calls export makes:
/// `heard`, `audio` as base64 μ-law) through the configured recognizer, as a call streams
/// them, with the time from the end of speech to the transcript.
async fn stt_probe(file: &Path, limit: usize, keyterms: &[String]) -> anyhow::Result<()> {
    use base64::Engine as _;
    let stt = speech_to_text();
    println!("recognizer: {}", stt.name());
    let text = std::fs::read_to_string(file)?;
    let bs = char::from(92).to_string();
    for (n, line) in text.lines().filter(|l| !l.trim().is_empty()).take(limit).enumerate() {
        let row: serde_json::Value = serde_json::from_str(&line.replace(&bs.repeat(2), &bs))?;
        let b64: String = row["audio"].as_str().unwrap_or("").chars().filter(|c| !c.is_whitespace()).collect();
        let audio = base64::engine::general_purpose::STANDARD.decode(b64)?;
        let mut session = stt.open("he-IL", keyterms).await?;
        for frame in audio.chunks(160) {
            session.input.send(callora_runtime::ports::SttInput::Audio(bytes::Bytes::copy_from_slice(frame))).await?;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let ended = std::time::Instant::now();
        session.input.send(callora_runtime::ports::SttInput::Finalize).await?;
        let mut heard = String::new();
        while let Ok(Some(e)) = tokio::time::timeout(Duration::from_secs(8), session.events.recv()).await {
            match e {
                callora_runtime::ports::SttEvent::Final(t) => {
                    heard = t;
                    break;
                }
                callora_runtime::ports::SttEvent::Error(e) => {
                    heard = format!("ERROR {e}");
                    break;
                }
                callora_runtime::ports::SttEvent::Closed => break,
                callora_runtime::ports::SttEvent::Unsure(_) => {}
                callora_runtime::ports::SttEvent::Partial(_) => {}
            }
        }
        let ms = ended.elapsed().as_millis();
        let _ = session.input.send(callora_runtime::ports::SttInput::Close).await;
        println!("{:>3} {:>5} ms | before: {} | now: {}", n + 1, ms, row["heard"].as_str().unwrap_or(""), heard);
    }
    Ok(())
}

/// The second hearing of a street or city answer (`SECOND_HEARING`): by default an OpenAI
/// audio model told the names expected (`SECOND_HEARING_MODEL`, default gpt-audio-1.5), which
/// on recorded and synthetic answers found the street 85-95% of the time where the stream
/// alone found 50-77%; `scribe` (or the old `1`) is ElevenLabs Scribe hinted with the names,
/// which made names up on live calls; `off` is none.
fn second_hearing() -> Option<Arc<dyn callora_runtime::ports::Transcriber>> {
    match env("SECOND_HEARING").as_deref().map(str::trim) {
        Some("off" | "0" | "none" | "false") => None,
        Some("scribe" | "1") => env("ELEVENLABS_API_KEY").map(|key| {
            Arc::new(callora_providers::scribe_batch::ScribeBatch::new(
                http(),
                key,
                None,
                env("ELEVENLABS_BATCH_STT_MODEL"),
            )) as Arc<dyn callora_runtime::ports::Transcriber>
        }),
        _ => env("OPENAI_API_KEY").map(|key| {
            tracing::info!("street and city answers get a second hearing by an audio model told the names");
            Arc::new(callora_providers::openai_audio::AudioHearing::new(http(), key, None, env("SECOND_HEARING_MODEL")))
                as Arc<dyn callora_runtime::ports::Transcriber>
        }),
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
    let second_hearing = second_hearing();
    let settings_store = Arc::new(match &db {
        Some(pool) => callora_runtime::settings::SettingsStore::load(pool).await.unwrap_or_else(|e| {
            tracing::error!(error = %e, "saved settings unreadable; using the business files");
            Default::default()
        }),
        None => Default::default(),
    });
    // The agent's model, switchable from the settings page.
    let agent_control = switchable_agent(client.clone(), &settings_store);
    let agent = agent_control.as_ref().map(|c| c.model());
    if let Some(control) = agent_control {
        settings_store.attach_agent_control(control);
    } else if registry.all().any(|b| b.config.agent.is_some()) {
        tracing::warn!("no key for AGENT_MODEL's provider: businesses with an agent fall back to the rules");
    }
    let mut actions = ConfiguredActions::new(client.clone(), env_snapshot());
    if let Some((url, token)) = env("WHATSAPP_URL").zip(env("WHATSAPP_TOKEN").filter(|t| t.len() >= 16)) {
        actions = actions.with_chat_bot(Arc::new(callora_runtime::whatsapp::WhatsAppChatBot::new(
            callora_runtime::whatsapp::Service::new(url, token),
            settings_store.clone(),
            db.clone(),
        )));
    }
    let services = Services {
        stt,
        llm,
        agent,
        second_hearing,
        gazetteer,
        tts,
        tts_cache: TtsCache::new(env("TTS_CACHE_ENTRIES").and_then(|v| v.parse().ok()).unwrap_or(2000)),
        actions: Arc::new(actions),
        telephony,
        store,
        whisper: Arc::new(NoWhisper),
        metrics: Arc::new(Metrics::default()),
        settings: settings_store,
        desk: None,
    };
    let mut session = SessionConfig { dynamic_model: env("ELEVENLABS_DYNAMIC_MODEL"), ..SessionConfig::default() };
    if let Some(ms) = env("VAD_ENDPOINT_MS").and_then(|v| v.parse().ok()) {
        session.vad.endpoint_ms = ms;
    }
    // On unless turned off: the agent starts on the recognizer's partial text at the end of
    // speech, ~350 ms before the final transcript; a different final starts it over.
    session.agent_speculate = !env("AGENT_SPECULATE").is_some_and(|v| v == "false" || v == "0");
    session.sample_audio_from = env("AUDIO_SAMPLE_NUMBERS").map(|l| parse_allow_list(&l)).unwrap_or_default();
    if let Some(ms) = env("VAD_TRIGGER_MS").and_then(|v| v.parse().ok()) {
        session.vad.trigger_ms = ms;
    }
    apply_voice_env(&mut session);

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
        // No password, no sign-in: a default one would let anyone read every call and, from the
        // settings, send callers to a number of their choosing.
        dashboard_password: env("DASHBOARD_PASSWORD").filter(|p| p.chars().count() >= 8).unwrap_or_else(|| {
            tracing::error!("DASHBOARD_PASSWORD is not set (8+ characters): nobody can sign in to the dashboard");
            String::new()
        }),
        web_dir: web_dir(),
        whatsapp: env("WHATSAPP_URL").zip(env("WHATSAPP_TOKEN").filter(|t| t.len() >= 16)),
        library_dir: Some(library_dir.clone()),
        library_model: env("ELEVENLABS_LIBRARY_MODEL"),
        eleven_agents: env("ELEVENLABS_API_KEY").map(|key| (key, env("ELEVENLABS_API_BASE_URL"))),
    };
    let state = AppState::new(registry, libraries, services, session, settings, db);
    // A voice chosen on the settings page replaces the business's own.
    state.apply_saved_voices();
    // Orders to WhatsApp, drained in the background at the accounts' pace.
    match (&state.db, &state.whatsapp) {
        (Some(pool), Some(service)) => {
            tokio::spawn(callora_runtime::whatsapp::run_sender(pool.clone(), service.clone()));
            tracing::info!("whatsapp sending on");
        }
        (_, None) => tracing::info!("no WHATSAPP_URL/WHATSAPP_TOKEN: whatsapp off"),
        (None, Some(_)) => tracing::warn!("whatsapp needs the database; off"),
    }
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
    // Cases with the caller's voice are heard as a call hears them.
    let hearing = cases
        .iter()
        .any(|c| c.turns.iter().any(|t| t.audio.is_some()))
        .then(|| eval::Hearing { stt: speech_to_text(), second: second_hearing() });
    let runner = Arc::new(eval::Runner {
        registry,
        gazetteer: load_gazetteer(),
        hearing,
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

    fn area() -> Vec<String> {
        let text = include_str!("../../../businesses/taxi.json");
        let v: serde_json::Value = serde_json::from_str(text).expect("taxi.json");
        v["service_area"]
            .as_array()
            .expect("service_area")
            .iter()
            .filter_map(|t| t.as_str().map(String::from))
            .collect()
    }

    /// Live calls' first sentences, with the real list: the area's towns however they were
    /// written, and nothing in everyday words.
    #[test]
    fn the_service_area_is_heard_in_live_mishearings() {
        std::env::set_var("STREETS_FILE", concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/israel-streets.tsv.gz"));
        std::env::set_var("PLACES_FILE", concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/israel-places.tsv.gz"));
        let g = super::load_gazetteer().expect("the list loads");
        let area = area();
        let towns = |t: &str| g.area_towns_heard(t, &area).into_iter().map(|(_, town)| town).collect::<Vec<_>>();
        for (said, town) in [
            ("מלאד לירושלים", "אלעד"),
            ("מלאדי, ירושלים.", "אלעד"),
            ("בלד לירושלים.", "אלעד"),
            ("מלעד לבנבר.", "אלעד"),
            ("מלעד לבנבר.", "בני ברק"),
            ("מלאד לבנברג.", "בני ברק"),
            ("בנברץ, עזרא 11.", "בני ברק"),
        ] {
            assert!(towns(said).iter().any(|t| t == town), "{said}: {:?}", g.area_towns_heard(said, &area));
        }
        for said in [
            "אני רוצה להזמין מונית.",
            "כן, אני רוצה",
            "יש פרטים נוספים",
            "שלושה נוסעים",
            "מאלעד לבני ברק",
            "תודה רבה",
            // Each was taken for a town before the rules were measured on every caller sentence.
            "שלום. ביי.",
            "לא, רושם.",
            "רחוב של מה",
            "על הקושי לאמא שלך.",
            "מי היה ראש הממשלה הראשון",
        ] {
            assert!(towns(said).is_empty(), "{said}: {:?}", g.area_towns_heard(said, &area));
        }
    }

    /// Every caller sentence in CALLER_LINES (one per line), with what it would be taken for.
    #[test]
    #[ignore]
    fn sweep_caller_lines() {
        std::env::set_var("STREETS_FILE", concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/israel-streets.tsv.gz"));
        std::env::set_var("PLACES_FILE", concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/israel-places.tsv.gz"));
        let g = super::load_gazetteer().expect("the list loads");
        let area = area();
        let lines = std::fs::read_to_string(std::env::var("CALLER_LINES").expect("CALLER_LINES")).expect("lines");
        for line in lines.lines() {
            let hits = g.area_towns_heard(line, &area);
            if !hits.is_empty() {
                println!("{line}  =>  {hits:?}");
            }
        }
    }

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
        // House numbers in words, as OpenAI's recognizer writes them.
        assert_eq!(spoken("בן זכאי ארבעים וחמש, אלעד"), "בן זכאי 45, אלעד");
        // "street, city" as the agent passes it: looked up in that city.
        match g.resolve_within("רחוב בן זכאי שלושים ושתיים", "אלעד") {
            Some(Lookup::Found(a)) => assert_eq!(a.spoken(), "בן זכאי 32, אלעד"),
            other => panic!("{other:?}"),
        }
        assert_eq!(spoken("באר שבע"), "באר שבע", "a name made of number words stays a name");
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

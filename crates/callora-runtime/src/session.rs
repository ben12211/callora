//! One actor per call.
//!
//! The session joins the caller's audio, the streaming STT, understanding (fast path and,
//! when needed, the LLM), the engine, business actions and playout. It is a single task
//! reacting to events, and every slow thing (STT connect, LLM, TTS, actions, the database,
//! Twilio REST) runs off to the side and reports back as an event, so nothing ever blocks
//! the audio: the greeting plays while STT is still connecting, a cached reply starts on
//! the next frame, and a barge-in cancels playout on the frame it is detected.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use callora_audio::library::VoiceLibrary;
use callora_audio::playout::{OutFrame, PlayItem, Playout, PlayoutConfig, PlayoutEvent, Source, TrimConfig};
use callora_audio::tts::{Synthesizer, TtsCache, TtsRequest};
use callora_audio::vad::{Vad, VadConfig, VadEvent};
use callora_core::agent::{self, AgentAction, SayStream};
use callora_core::business::Business;
use callora_core::customer::Customer;
use callora_core::engine::{Directive, Engine, HandoffSummary};
use callora_core::llm;
use callora_core::render::{SegmentOrigin, SpeechPlan, SpeechSegment};
use callora_core::speech::prepare_for_tts;
use callora_core::state::Speaker;
use callora_core::understanding::{fast_path, merge, Understanding};

use crate::barge::{classify, classify_final, suppressed_label, BargeConfig, BargeInput, BargeReason, Phase};
use crate::metrics::Metrics;
use crate::ports::{
    ActionRunner, CallInfo, CallRecord, CallStore, LanguageModel, SpeechToText, SttEvent, SttInput, SttSession,
    Telephony, Usage, WhisperRegistry,
};

mod agent_turn;
mod hearing;
mod speech;

/// Up to this many words that nothing understood are treated as noise when the LLM cannot
/// be asked.
const SHORT_GARBAGE_WORDS: usize = 3;
/// How long an unfinished sentence waits for the caller to go on.
const UNFINISHED_WAIT: Duration = Duration::from_millis(1200);
/// After words taken for line noise and nothing more, how long before the question is asked
/// again: a
/// live call waited 11 seconds in silence after its "שלום" (likely "שלוש") was dropped.
const UNHEARD_WAIT: Duration = Duration::from_millis(2000);

// How long the caller's voice must go on over the agent before it stops lives in
// `crate::barge::BargeConfig`: a cough, a car horn or the TV cut the agent off mid-question
// in live calls, and 450 ms was a "הלו" over the greeting, which then stopped mid-sentence.
/// After speech with no words at all (noise the recognizer returned nothing for), how long
/// before the question is asked again: otherwise nothing is said until the caller speaks.
const NO_WORDS_WAIT: Duration = Duration::from_millis(2000);
/// The longest a turn waits for its second hearing before the agent goes on without it. The
/// audio hearing takes about a second (p90 1.4 s on 161 street answers).
const SECOND_HEARING_WAIT: Duration = Duration::from_millis(1800);
/// Words this much quieter than the caller's own voice so far are probably someone near them.
/// On noisy clips, someone talking near the caller fell under it 9 times in 12; on 542 utterances
/// of past calls, 8 of the callers' own did.
const DISTANT_RATIO: f32 = 0.3;
/// A reply that began this long after the caller's voice ended is a slow turn (call health).
const SLOW_TURN_MS: u64 = 3500;
/// Lost audio within an utterance from which the agent is told words may be missing.
const LINE_LOST_NOTE_MS: u64 = 200;
/// A frame with this share of its samples at the top of the scale is clipped.
const CLIPPED_SHARE: f32 = 0.1;
/// An utterance with this share of clipped voiced frames is distorted.
const DISTORTED_SHARE: f32 = 0.2;
/// A word the recognizer gives less than this probability is told to the agent as unsure.
const UNSURE_BELOW: f32 = 0.5;
/// Speech that begins this soon after the agent stopped may still be its echo (the phone's
/// own delay); later it is the caller answering, often with the agent's words ("בבני ברק").
const ECHO_TAIL: Duration = Duration::from_millis(300);
/// Words begun before the agent's reply finish the previous answer only when they follow it
/// closely ("דוד" ... "אביטבול"), not after a long pause.
const CONTINUATION_GAP: Duration = Duration::from_millis(2500);

/// Consecutive speech recognition reconnects (with no transcript in between) before the
/// call is handed off.
const MAX_STT_RECONNECTS: u32 = 4;
/// A yes said this close to the end of the read-back ("כן, תשלח" over its "לשלוח?") is the
/// answer: a live caller said "כן" six times over a long one and had to say it again after.
const YES_AT_THE_END: Duration = Duration::from_millis(2500);
/// How long the transcript of an end of speech may take before recognition is taken for
/// stuck and reconnected.
const FINAL_OVERDUE: Duration = Duration::from_secs(6);

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub vad: VadConfig,
    /// Frames sent ahead of real time (3 = 60 ms).
    pub lead_frames: u32,
    /// How long the greeting may wait for the customer lookup started by the webhook.
    pub customer_wait: Duration,
    pub max_call: Duration,
    /// Overrides the business's dynamic TTS model (e.g. from `ELEVENLABS_DYNAMIC_MODEL`).
    pub dynamic_model: Option<String>,
    /// Inbound audio kept while the STT connects.
    pub stt_buffer_frames: usize,
    /// Start the agent on the recognizer's partial text at the end of speech. Off by default:
    /// on live calls the partial rarely matched the final transcript (1 turn in ~15), and
    /// every miss spends a full request against the account's tokens-per-minute limit.
    ///
    /// (`SessionConfig::default()` leaves it off; the `callora` binary turns it on unless
    /// `AGENT_SPECULATE=false`.)
    pub agent_speculate: bool,
    /// Gain added to everything the agent says, in dB (library clips, cached and live TTS).
    /// 0 plays the audio untouched. The caller's "speak louder" request adds to it.
    pub tts_gain_db: f32,
    /// The most gain in total, base plus "speak louder".
    pub tts_gain_max_db: f32,
    /// The loudest the output may be after gain (dBFS); the limiter keeps it under.
    pub limiter_ceiling_dbfs: f32,
    /// What decides that the caller's voice is an interruption.
    pub barge: BargeConfig,
    /// A sentence with at most this much left to play is let finish when the caller
    /// interrupts. 0 cuts at once, as before.
    pub sentence_end_protect_ms: u64,
    /// The silence that ends an utterance when the caller has plainly finished a short
    /// answer, and when the sentence sounds unfinished. Equal to `vad.endpoint_ms`: no
    /// adaptation.
    pub endpoint_short_ms: u64,
    pub endpoint_long_ms: u64,
    /// Speech a live TTS sentence buffers before it starts: the first of a reply, and the
    /// ones that follow other audio of the same reply.
    pub tts_start_buffer_ms: u32,
    pub tts_continuation_buffer_ms: u32,
    pub tts_rebuffer_ms: u32,
    /// Silence at the ends of a sentence is cut to this much. `None`: audio plays as is.
    pub trim_silence: Option<TrimConfig>,
    /// A silence this long between two pieces of one reply is logged as a warning.
    pub audio_gap_warn_ms: u64,
    /// Longer than this is a pause (a reprompt, a turn), not a hole in a reply.
    pub audio_gap_ignore_ms: u64,
    /// Callers (E.164) whose utterances are kept as audio, to compare recognizers. Empty:
    /// no audio is kept.
    pub sample_audio_from: Vec<String>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            vad: VadConfig::default(),
            lead_frames: 3,
            customer_wait: Duration::from_millis(150),
            max_call: Duration::from_secs(15 * 60),
            dynamic_model: None,
            stt_buffer_frames: 150,
            agent_speculate: false,
            // +4 dB: measured nothing yet on live calls; the limiter makes it safe, and a
            // business that plays too loud turns it down with TTS_GAIN_DB.
            tts_gain_db: 4.0,
            tts_gain_max_db: 12.0,
            limiter_ceiling_dbfs: -1.5,
            barge: BargeConfig::default(),
            sentence_end_protect_ms: 400,
            endpoint_short_ms: 350,
            endpoint_long_ms: 700,
            tts_start_buffer_ms: 250,
            tts_continuation_buffer_ms: 120,
            tts_rebuffer_ms: 250,
            trim_silence: Some(TrimConfig { threshold_rms: 60.0, pad_ms: 60, tail_pad_ms: 100 }),
            audio_gap_warn_ms: 120,
            audio_gap_ignore_ms: 2500,
            sample_audio_from: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub struct Services {
    pub stt: Arc<dyn SpeechToText>,
    pub llm: Option<Arc<dyn LanguageModel>>,
    /// The conversation agent's model; with a business `agent` config it runs the call.
    pub agent: Option<Arc<dyn LanguageModel>>,
    /// Israel's localities and streets, for checking places.
    pub gazetteer: Option<Arc<callora_core::gazetteer::Gazetteer>>,
    /// A second, slower transcription of doubtful utterances (a city or street expected).
    pub second_hearing: Option<Arc<dyn crate::ports::Transcriber>>,
    pub tts: Option<Arc<dyn Synthesizer>>,
    pub tts_cache: TtsCache,
    pub actions: Arc<dyn ActionRunner>,
    pub telephony: Arc<dyn Telephony>,
    pub store: Arc<dyn CallStore>,
    pub whisper: Arc<dyn WhisperRegistry>,
    pub metrics: Arc<Metrics>,
    /// The owner's settings (the dispatch desk).
    pub settings: Arc<crate::settings::SettingsStore>,
    /// Hands callers to the desk with hold music; set by the server.
    pub desk: Option<Arc<crate::desk::Desk>>,
}

#[derive(Debug)]
pub enum Inbound {
    /// μ-law from the caller.
    Audio(Bytes),
    /// The caller's audio skipped this many ms (the line cut out).
    Gap(u64),
    /// The media stream ended.
    Stop,
}

/// How the session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ending {
    CallerHungUp,
    AgentHungUp,
    HandedOff,
    TimeLimit,
}

enum Ev {
    SttReady(anyhow::Result<SttSession>),
    /// A recognition session biased with a city's streets (or, with no city, with the
    /// business's words only), opened beside the live one.
    SttFocused {
        city: Option<String>,
        result: anyhow::Result<SttSession>,
    },
    /// The second hearing of an utterance (or its failure / timeout).
    SecondHearing {
        id: u64,
        text: Option<String>,
    },
    Llm {
        turn: u64,
        result: anyhow::Result<Value>,
        elapsed: Duration,
    },
    FillerDue {
        turn: u64,
    },
    /// A finished sentence of the agent's reply, while the rest is still being generated.
    AgentSay {
        turn: u64,
        sentence: String,
    },
    /// The agent's whole decision (and the tail of `say` that had no sentence mark).
    /// The reply's recorded phrase, as soon as its id is complete.
    AgentPhrase {
        turn: u64,
        id: String,
    },
    /// The reply's fields, before any of its words.
    AgentFields {
        turn: u64,
        fields: Vec<(String, String)>,
    },
    AgentAsks {
        turn: u64,
        asks: Vec<String>,
    },
    AgentDone {
        turn: u64,
        result: anyhow::Result<Value>,
        rest: Option<String>,
        usage: Option<Usage>,
    },
    Action {
        run_id: u64,
        action: String,
        input: Value,
        result: Result<Value, callora_core::engine::ActionFailure>,
        elapsed: Duration,
    },
    Silence {
        generation: u64,
    },
    TtsFirstChunk {
        elapsed: Duration,
    },
    /// An unfinished sentence ("ואני רוצה להגיע ל...") waited long enough for its rest.
    UnfinishedDue {
        generation: u64,
    },
    /// A hangup or handoff with nothing left to say.
    TerminateNow,
    /// The transcript asked for at this end of speech, checked for.
    FinalOverdue {
        sent_at: Instant,
    },
    /// Words taken for noise, then nothing: ask the question again.
    Unheard {
        generation: u64,
    },
    /// Speech that ended some time ago and brought no words at all.
    NoWords {
        utterance: u64,
        generation: u64,
    },
}

enum AfterSpeech {
    Hangup,
    Handoff(HandoffSummary),
}

struct PendingLlm {
    turn: u64,
    transcript: String,
    fast: Understanding,
    task: JoinHandle<()>,
}

/// An agent decision in progress.
struct PendingAgent {
    turn: u64,
    transcript: String,
    task: JoinHandle<()>,
    started: Instant,
    /// Started on the recognizer's partial text before the final transcript: its speech is
    /// held back until the final transcript says the same.
    speculative: bool,
    /// Sentences already played.
    spoken: Vec<String>,
    /// Sentences held back while speculative.
    held: Vec<String>,
    /// The recorded phrase held back while speculative.
    held_phrase: Option<String>,
    /// The recorded phrase already played.
    phrase: Option<String>,
    /// The values the reply passes (known before its words).
    fields: Vec<(String, String)>,
    /// The start of what may be a recorded phrase ("הכל טוב, תודה!" of "הכל טוב, תודה!
    /// איך אפשר לעזור?"), waiting for its next sentence so the whole clip plays.
    partial_phrase: Option<String>,
    /// The decision, when it finished while still speculative.
    done: Option<(anyhow::Result<Value>, Option<String>, Option<Usage>)>,
    /// A value in the reply will be rejected: its words move on without it, so none play
    /// and the engine asks for the value again.
    hold_say: bool,
    /// What the reply's question asks for (known before its words).
    asks: Vec<String>,
    /// The detail asked for earlier and still missing that this reply moved on past: its
    /// words were held and the engine asks for the detail again.
    held_for: Option<String>,
}

/// Where one reply's time went, from the end of the caller's speech to the first audio.
struct TurnClock {
    speech_end: Instant,
    /// When the caller's last sound was (the VAD ends an utterance after the endpoint
    /// silence): `speech_end` minus the silence waited.
    voice_end: Instant,
    /// How long the caller spoke, and how loud (mean and peak RMS).
    speech_ms: u64,
    rms_mean: f32,
    rms_peak: f32,
    final_at: Option<Instant>,
    agent_first: Option<Instant>,
    tts_first_byte: Option<Instant>,
    speculative_hit: bool,
    audio: &'static str,
}

pub struct Session {
    business: Arc<Business>,
    library: Arc<VoiceLibrary>,
    services: Services,
    cfg: SessionConfig,
    info: CallInfo,
    engine: Engine,
    playout: Playout,
    vad: Vad,
    /// RNNoise on the caller's track, for the VAD's voice probability (when it uses one).
    denoiser: Option<callora_audio::denoise::Denoiser>,
    events: mpsc::UnboundedSender<Ev>,
    stt: Option<SttSession>,
    stt_backlog: VecDeque<Bytes>,
    speaking: bool,
    /// Items enqueued and neither finished nor cancelled: the agent "has the floor".
    queued_items: usize,
    next_item: u64,
    turn: u64,
    pending_llm: Option<PendingLlm>,
    actions_in_flight: usize,
    after_speech: Option<AfterSpeech>,
    silence_generation: u64,
    speech_ended_at: Option<Instant>,
    /// When the caller's current utterance began (VAD), and when the agent last started a
    /// reply: an utterance begun before the reply answers the question before it.
    speech_started_at: Option<Instant>,
    reply_started_at: Option<Instant>,
    /// When the agent's audio last stopped playing.
    agent_idle_at: Option<Instant>,
    /// The voiced frames of the current utterance: their summed RMS and count.
    voiced_level: (f32, u32),
    /// The last utterance's voice level (mean RMS of its voiced frames).
    utterance_level: f32,
    /// The levels of the caller's own utterances so far (the ones that were words).
    caller_levels: Vec<f32>,
    /// The next final transcript is held words released as they are (not held again).
    releasing_unfinished: bool,
    /// The recognizer closed with the caller's last words unanswered: the next one hears them.
    resend_utterance: bool,
    /// Turns whose reply began more than SLOW_TURN_MS after the caller's voice ended.
    slow_turns: u32,
    /// The words of the coming transcript the recognizer was unsure of.
    unsure: Vec<(String, f32)>,
    /// The current utterance: audio the line lost (ms), and its voiced frames distorted by
    /// clipping (wind on the microphone, shouting).
    line_lost_ms: u64,
    clipped_frames: u32,
    /// The caller's current speech began while the agent was talking or just after: it may be
    /// the agent's own voice coming back through a speakerphone.
    speech_over_agent: bool,
    /// The next agent request is told the caller's words overlap its last reply.
    overlap: bool,
    /// The route whose price list was asked ahead of the caller.
    priced: Option<(String, String)>,
    barge_in_started: Option<Instant>,
    /// A sentence nearly over was let finish after an interruption (until the playout idles).
    protecting: bool,
    /// Why the voice over the agent has not stopped it yet, for the metric.
    barge_hint: &'static str,
    /// The caller utterance count at the last audio of the agent's: a gap with the caller
    /// speaking in between is a turn, not a hole in a reply.
    audio_speech_count: u64,
    /// The agent was cut off and no real utterance has followed yet.
    interrupted: bool,
    /// The caller's voice began over the agent, which goes on until words (or a long enough
    /// voice) show it is not noise.
    barge_pending: bool,
    /// What the recognizer has heard of the voice over the agent: real words, and whether it
    /// is only a listening sound.
    barge_words: (usize, bool),
    /// Audio of the current utterance so far, in ms (8 kHz μ-law: 8 bytes a millisecond).
    voiced_ms: u64,
    /// A read-back was cut off: a "yes" now was said without hearing all of it.
    cut_read_back: bool,
    /// The current utterance (counted at each start of speech) and whether it brought words.
    speech_count: u64,
    utterance_heard: bool,
    no_words_generation: u64,
    no_words_task: Option<JoinHandle<()>>,
    /// When the caller last stopped speaking, and the pause before the current utterance.
    last_speech_end: Option<Instant>,
    speech_gap: Option<Duration>,
    /// When the last finalize went to the STT, for the transcription latency metric.
    finalize_sent_at: Option<Instant>,
    /// Reconnects since the last transcript.
    stt_reconnects: u32,
    /// The city whose streets bias recognition now, the one being opened, and a session
    /// ready to take over once the caller is between utterances.
    stt_city: Option<String>,
    /// The caller's audio: the last 300 ms before speech, the utterance being spoken, and the
    /// last one finished; and the transcript waiting for its second hearing.
    preroll: std::collections::VecDeque<Bytes>,
    utterance: Vec<u8>,
    last_utterance: Vec<u8>,
    second_pending: Option<(u64, String)>,
    second_ids: u64,
    stt_opening: Option<Option<String>>,
    stt_next: Option<(Option<String>, SttSession)>,
    pending_agent: Option<PendingAgent>,
    /// The recognizer's latest partial text for the utterance in progress.
    last_partial: String,
    /// A yes said over the read-back, and when: taken as the answer if it came at its end.
    yes_over_read_back: Option<(Instant, String)>,
    /// The read-back the caller has heard to its end, word for word.
    read_back_heard: Option<String>,
    clock: Option<TurnClock>,
    /// The last sentence queued to play, the context of the next one of the same reply.
    last_segment_text: Option<String>,
    /// The caller turn the live-TTS cover last played on (never two turns in a row).
    cover_turn: Option<u32>,
    /// The agent's recorded phrases, as words (see [`Session::agent_sentence`]).
    phrase_words: Vec<Vec<String>>,
    /// A transcript that ended mid-sentence, waiting for the caller to go on.
    unfinished: Option<String>,
    unfinished_generation: u64,
    /// The caller turn (engine turn count) the thinking filler last played on. A filler
    /// on every turn sounds scripted, so it never plays on two turns in a row.
    filler_turn: Option<u32>,
    /// What the agent's decisions cost in this call.
    usage: Usage,
    /// Live speech and second hearings in this call.
    meter: crate::ports::Meter,
}

impl Session {
    /// Run the call to completion.
    #[allow(clippy::too_many_arguments)]
    pub async fn run(
        business: Arc<Business>,
        library: Arc<VoiceLibrary>,
        services: Services,
        cfg: SessionConfig,
        info: CallInfo,
        customer: Option<oneshot::Receiver<Option<Customer>>>,
        mut inbound: mpsc::Receiver<Inbound>,
        outbound: mpsc::UnboundedSender<OutFrame>,
    ) -> Ending {
        let metrics = services.metrics.clone();
        metrics.calls_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        metrics.calls_active.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let (pl_tx, mut pl_rx) = mpsc::unbounded_channel();
        let (playout, playout_task) = Playout::spawn_with(
            outbound,
            pl_tx,
            PlayoutConfig {
                lead_frames: cfg.lead_frames,
                limiter_ceiling_dbfs: cfg.limiter_ceiling_dbfs,
                start_buffer_ms: cfg.tts_start_buffer_ms,
                continuation_buffer_ms: cfg.tts_continuation_buffer_ms,
                rebuffer_ms: cfg.tts_rebuffer_ms,
                trim: cfg.trim_silence,
            },
        );
        let seed = info.call_id.as_u128() as u64;
        let mut engine = Engine::new(business.clone(), seed);
        engine.set_gazetteer(services.gazetteer.clone());
        engine.set_caller_phone(info.from.clone());
        engine.set_desk(!services.settings.desk(&business).numbers.is_empty());
        let mut s = Session {
            engine,
            business,
            library,
            vad: Vad::new(cfg.vad),
            denoiser: cfg.vad.voice_start.map(|_| callora_audio::denoise::Denoiser::new()),
            services,
            cfg,
            info,
            playout,
            events: ev_tx,
            stt: None,
            stt_backlog: VecDeque::new(),
            speaking: false,
            queued_items: 0,
            next_item: 1,
            turn: 0,
            pending_llm: None,
            actions_in_flight: 0,
            after_speech: None,
            silence_generation: 0,
            speech_ended_at: None,
            speech_started_at: None,
            reply_started_at: None,
            agent_idle_at: None,
            voiced_level: (0.0, 0),
            utterance_level: 0.0,
            caller_levels: Vec::new(),
            unsure: Vec::new(),
            releasing_unfinished: false,
            resend_utterance: false,
            slow_turns: 0,
            line_lost_ms: 0,
            clipped_frames: 0,
            speech_over_agent: false,
            overlap: false,
            priced: None,
            barge_in_started: None,
            protecting: false,
            barge_hint: "noise",
            audio_speech_count: 0,
            interrupted: false,
            barge_pending: false,
            barge_words: (0, false),
            voiced_ms: 0,
            cut_read_back: false,
            speech_count: 0,
            utterance_heard: false,
            no_words_generation: 0,
            no_words_task: None,
            last_speech_end: None,
            speech_gap: None,
            finalize_sent_at: None,
            stt_reconnects: 0,
            stt_city: None,
            preroll: std::collections::VecDeque::new(),
            utterance: Vec::new(),
            last_utterance: Vec::new(),
            second_pending: None,
            second_ids: 0,
            stt_opening: None,
            stt_next: None,
            pending_agent: None,
            last_partial: String::new(),
            yes_over_read_back: None,
            read_back_heard: None,
            clock: None,
            last_segment_text: None,
            cover_turn: None,
            phrase_words: Vec::new(),
            unfinished: None,
            unfinished_generation: 0,
            filler_turn: None,
            usage: Usage::default(),
            meter: crate::ports::Meter::default(),
        };
        s.phrase_words = agent::phrases(&s.business).iter().map(|p| agent_turn::words(p)).collect();
        s.services.store.record(CallRecord::Started { info: s.info.clone() });

        // Open the agent's and the voice's connections while the greeting plays, so the
        // first reply does not pay for TLS handshakes.
        if let (Some(a), true) = (s.services.agent.clone(), s.business.config.agent.is_some()) {
            // A throwaway decision loads the agent's instructions into the provider's prompt
            // cache while the greeting plays: after a few idle minutes the cache is cold,
            // and the first real turn of a call was the slowest (over a second).
            let request = agent::build_request(&s.business, &s.engine.state, "…");
            tokio::spawn(async move {
                a.warm().await;
                let _ = a.extract(&request).await;
            });
        }
        if let Some(t) = s.services.tts.clone() {
            tokio::spawn(async move { t.warm().await });
        }

        // STT connects in the background; the greeting does not wait for it.
        {
            let stt = s.services.stt.clone();
            let language = s.business.config.language.clone();
            let keyterms = s.stt_keyterms(None);
            let tx = s.events.clone();
            tokio::spawn(async move {
                let _ = tx.send(Ev::SttReady(stt.open(&language, &keyterms).await));
            });
        }

        // The customer lookup was started by the voice webhook; give it a moment, then greet.
        let mut customer = customer;
        if let Some(rx) = customer.as_mut() {
            if let Ok(Ok(c)) = tokio::time::timeout(s.cfg.customer_wait, rx).await {
                s.engine.set_customer(c);
                customer = None;
            }
        }
        let greeting = s.engine.start();
        s.execute(greeting);

        let deadline = tokio::time::sleep(s.cfg.max_call);
        tokio::pin!(deadline);
        let ending = loop {
            let stt_events = async {
                match s.stt.as_mut() {
                    Some(stt) => stt.events.recv().await,
                    None => std::future::pending().await,
                }
            };
            let customer_late = async {
                match customer.as_mut() {
                    Some(rx) => rx.await.ok().flatten(),
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                msg = inbound.recv() => match msg {
                    Some(Inbound::Audio(frame)) => s.on_audio(frame),
                    Some(Inbound::Gap(ms)) => s.on_line_gap(ms),
                    Some(Inbound::Stop) | None => break Ending::CallerHungUp,
                },
                Some(e) = pl_rx.recv() => {
                    if let Some(end) = s.on_playout(e).await {
                        break end;
                    }
                }
                e = stt_events => s.on_stt(e.unwrap_or(SttEvent::Closed)),
                Some(e) = ev_rx.recv() => {
                    if matches!(e, Ev::TerminateNow) {
                        if let Some(after) = s.after_speech.take() {
                            break s.terminate(after).await;
                        }
                    } else {
                        s.on_event(e);
                    }
                }
                c = customer_late => {
                    customer = None;
                    s.engine.set_customer(c);
                }
                () = &mut deadline => {
                    tracing::warn!(call = %s.info.call_sid, "call reached the time limit");
                    let _ = s.services.telephony.hangup(&s.info.call_sid).await;
                    break Ending::TimeLimit;
                }
            }
        };

        s.cancel_no_words();
        if let Some(stt) = &s.stt {
            let _ = stt.input.try_send(SttInput::Close);
        }
        if let Some(p) = s.pending_llm.take() {
            p.task.abort();
        }
        if let Some(p) = s.pending_agent.take() {
            p.task.abort();
        }
        playout_task.abort();
        for card in callora_core::orders::order_cards(&s.business, &s.engine.state) {
            tracing::info!(call = %s.info.call_sid, summary = %card["summary"].as_str().unwrap_or(""), "order card");
            s.services.store.record(CallRecord::Order { call_id: s.info.call_id, card });
        }
        let outcome = format!("{ending:?}");
        s.services.store.record(CallRecord::Ended {
            call_id: s.info.call_id,
            outcome,
            state: serde_json::to_value(&s.engine.state).unwrap_or(Value::Null),
            usage: s.usage.clone(),
            meter: s.meter.clone(),
        });
        metrics.calls_active.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        tracing::info!(call = %s.info.call_sid, ?ending, turns = s.engine.state.turns, "call ended");
        // What went wrong in this call, counted and logged: problems are seen without anyone
        // calling to find them.
        let mut problems = s.engine.health();
        let unfinished = problems.contains(&"booking_left_unfinished");
        match ending {
            Ending::CallerHungUp if unfinished => problems.push("caller_hung_up_mid_booking"),
            Ending::AgentHungUp if unfinished => problems.push("agent_hung_up_mid_booking"),
            _ => {}
        }
        if s.slow_turns > 0 {
            problems.push("slow_turns");
        }
        for p in &problems {
            metrics.call_problems.inc(p);
        }
        if !problems.is_empty() {
            tracing::warn!(call = %s.info.call_sid, ?problems, slow_turns = s.slow_turns, "call health: problems in this call");
        }
        ending
    }

    fn on_event(&mut self, e: Ev) {
        match e {
            Ev::SttReady(Ok(session)) => {
                // The recognizer that closed had the caller's last words and never answered
                // them (it failed over to the backup): they are heard again, not lost.
                if std::mem::take(&mut self.resend_utterance) && !self.last_utterance.is_empty() {
                    tracing::info!(call = %self.info.call_sid, ms = self.last_utterance.len() / 8, "the caller's last words to the new recognizer");
                    for chunk in self.last_utterance.chunks(160) {
                        let _ = session.input.try_send(SttInput::Audio(Bytes::copy_from_slice(chunk)));
                    }
                    if session.input.try_send(SttInput::Finalize).is_ok() {
                        let now = Instant::now();
                        self.finalize_sent_at = Some(now);
                        let tx = self.events.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(FINAL_OVERDUE).await;
                            let _ = tx.send(Ev::FinalOverdue { sent_at: now });
                        });
                    }
                }
                for frame in self.stt_backlog.drain(..) {
                    let _ = session.input.try_send(SttInput::Audio(frame));
                }
                self.stt = Some(session);
            }
            Ev::SecondHearing { id, text } => {
                if self.second_pending.as_ref().map(|(i, _)| *i) != Some(id) {
                    return;
                }
                let Some((_, transcript)) = self.second_pending.take() else { return };
                self.engine.state.second_hearing =
                    text.filter(|t| !t.is_empty()).map(|t| self.engine.with_stream_number(&transcript, &t));
                self.start_agent(transcript, false);
            }
            Ev::SttFocused { city, result } => {
                if self.stt_opening.as_ref() == Some(&city) {
                    self.stt_opening = None;
                }
                match result {
                    // The question moved on while it opened: not the session wanted now.
                    Ok(session) if city != self.engine.street_focus() => {
                        let _ = session.input.try_send(SttInput::Close);
                        self.focus_stt();
                    }
                    Ok(session) => {
                        if let Some((_, stale)) = self.stt_next.replace((city, session)) {
                            let _ = stale.input.try_send(SttInput::Close);
                        }
                        self.swap_stt_if_ready();
                    }
                    Err(error) => {
                        tracing::warn!(call = %self.info.call_sid, city = ?city, %error, "city recognition hints unavailable")
                    }
                }
            }
            Ev::SttReady(Err(error)) => {
                tracing::error!(call = %self.info.call_sid, %error, "speech recognition unavailable");
                let d = self.engine.force_handoff("stt_unavailable");
                self.execute(d);
            }
            Ev::Llm { turn, result, elapsed } => {
                if self.pending_llm.as_ref().map(|p| p.turn) != Some(turn) {
                    return;
                }
                let Some(pending) = self.pending_llm.take() else { return };
                self.services.metrics.llm_latency.observe(elapsed.as_millis() as u64);
                let u = match result {
                    Ok(reply) => {
                        let parsed =
                            llm::parse_response(&self.business, &self.engine.context(), &pending.transcript, &reply);
                        merge(pending.fast, parsed)
                    }
                    Err(error) => {
                        self.services.metrics.llm_failures_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        tracing::warn!(call = %self.info.call_sid, error = %format!("{error:#}"), "llm understanding failed; using the fast path");
                        // Without the LLM's judgement, a few words the rules make nothing of
                        // are more likely line noise than a request: answering them with
                        // "didn't catch that" is the robotic reflex callers complained about.
                        let mut fast = pending.fast;
                        if fast.is_empty() && fast.transcript.split_whitespace().count() <= SHORT_GARBAGE_WORDS {
                            fast.noise = true;
                        }
                        fast
                    }
                };
                if u.noise {
                    return self.on_noise(&u.transcript);
                }
                self.understood(u);
            }
            Ev::FillerDue { turn } => {
                let caller_turn = self.engine.state.turns;
                let filler_last_turn = self.filler_turn.is_some_and(|t| t + 1 == caller_turn);
                let rules_waiting = self.pending_llm.as_ref().map(|p| p.turn) == Some(turn);
                let agent_waiting = self
                    .pending_agent
                    .as_ref()
                    .is_some_and(|p| p.turn == turn && !p.speculative && p.spoken.is_empty());
                let filler = if agent_waiting {
                    self.business.config.agent.as_ref().and_then(|a| a.thinking_filler.clone())
                } else {
                    self.business.config.understanding.thinking_filler.clone()
                };
                // Once a turn, and not two turns in a row: "שנייה, שנייה, שנייה" on every turn
                // sounded like a machine stalling.
                let filler_this_turn = self.filler_turn == Some(caller_turn);
                if (rules_waiting || agent_waiting) && !self.agent_busy() && !filler_last_turn && !filler_this_turn {
                    if let Some(id) = filler {
                        if let Some(plan) = self.engine.render_response(&id) {
                            self.filler_turn = Some(caller_turn);
                            self.speak(plan);
                        }
                    }
                }
            }
            Ev::Action { run_id, action, input, result, elapsed } => {
                self.actions_in_flight = self.actions_in_flight.saturating_sub(1);
                self.services.metrics.action_latency.observe(elapsed.as_millis() as u64);
                if result.is_err() {
                    self.services.metrics.action_failures_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                self.services.store.record(CallRecord::Action {
                    call_id: self.info.call_id,
                    action,
                    input,
                    result: match &result {
                        Ok(v) => v.clone(),
                        Err(e) => json!({ "error": e }),
                    },
                    ok: result.is_ok(),
                    latency_ms: elapsed.as_millis() as u64,
                });
                let d = self.engine.on_action_result(run_id, result);
                self.execute(d);
            }
            Ev::Silence { generation } => {
                if generation == self.silence_generation
                    && !self.vad.is_speaking()
                    && !self.agent_busy()
                    && self.pending_agent.is_none()
                    && self.pending_llm.is_none()
                    && self.second_pending.is_none()
                    && self.unfinished.is_none()
                    && self.actions_in_flight == 0
                    && self.after_speech.is_none()
                {
                    let d = self.engine.on_silence();
                    self.execute(d);
                }
            }
            Ev::Unheard { generation } => {
                if generation == self.silence_generation
                    && !self.vad.is_speaking()
                    && !self.agent_busy()
                    && self.pending_agent.is_none()
                    && self.pending_llm.is_none()
                    && self.actions_in_flight == 0
                    && self.after_speech.is_none()
                {
                    let d = self.engine.on_unheard();
                    if d.is_empty() {
                        self.arm_silence();
                    }
                    self.execute(d);
                }
            }
            Ev::NoWords { utterance, generation } => {
                if generation != self.no_words_generation || utterance != self.speech_count || self.utterance_heard {
                    return;
                }
                // A guess begun on partial words that never became a transcript: nothing will
                // finish it, and it kept the call silent until the caller spoke again.
                if self.pending_agent.as_ref().is_some_and(|p| p.speculative) {
                    if let Some(p) = self.pending_agent.take() {
                        tracing::info!(call = %self.info.call_sid, "a guess on words never confirmed; dropped");
                        p.task.abort();
                    }
                }
                if utterance == self.speech_count
                    && !self.utterance_heard
                    && !self.vad.is_speaking()
                    && !self.agent_busy()
                    && self.pending_agent.is_none()
                    && self.pending_llm.is_none()
                    && self.second_pending.is_none()
                    && self.unfinished.is_none()
                    && self.actions_in_flight == 0
                    && self.after_speech.is_none()
                    // The recognizer has not answered yet: its words may still come. A slow
                    // transcript (2.3 s) was answered "the line is noisy" and then cut it off.
                    && self.finalize_sent_at.is_none()
                {
                    self.cancel_no_words();
                    tracing::info!(call = %self.info.call_sid, "speech with no words; the line is noisy");
                    let interrupted = std::mem::take(&mut self.interrupted);
                    self.noise_heard(interrupted);
                } else if !self.vad.is_speaking() && self.after_speech.is_none() {
                    // A reply or second hearing may still be finishing. Do not lose recovery.
                    self.arm_no_words();
                }
            }
            Ev::TtsFirstChunk { elapsed } => {
                self.services.metrics.tts_first_chunk.observe(elapsed.as_millis() as u64);
                if let Some(c) = &mut self.clock {
                    c.tts_first_byte.get_or_insert_with(Instant::now);
                }
            }
            Ev::UnfinishedDue { generation } if generation == self.unfinished_generation && self.vad.is_speaking() => {
                // Not now, but not never: dropped, the held words blocked every reprompt.
                let tx = self.events.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(UNFINISHED_WAIT).await;
                    let _ = tx.send(Ev::UnfinishedDue { generation });
                });
            }
            Ev::UnfinishedDue { generation } => {
                if generation == self.unfinished_generation {
                    if let Some(text) = self.unfinished.take() {
                        // Answer what there is; strip the trailing marks so it is not held again.
                        let text = text.trim_end_matches(['.', '…', '-', ' ']).to_string();
                        if !text.is_empty() {
                            self.releasing_unfinished = true;
                            self.on_final(text);
                        }
                    }
                }
            }
            Ev::AgentSay { turn, sentence } => {
                let Some(p) = self.pending_agent.as_mut().filter(|p| p.turn == turn) else { return };
                if p.speculative {
                    p.held.push(sentence);
                    return;
                }
                self.agent_sentence(sentence);
            }
            Ev::AgentFields { turn, fields } => {
                let transcript = self.pending_agent.as_ref().map(|p| p.transcript.clone()).unwrap_or_default();
                let rejects = self.engine.rejects_any(&transcript, &fields);
                if let Some(p) = self.pending_agent.as_mut().filter(|p| p.turn == turn) {
                    p.fields.clone_from(&fields);
                    if rejects {
                        tracing::info!(call = %self.info.call_sid, ?fields, "a value will be rejected; the agent's words are held");
                        p.hold_say = true;
                    }
                }
            }
            Ev::AgentAsks { turn, asks } => {
                let (transcript, fields) =
                    self.pending_agent.as_ref().map(|p| (p.transcript.clone(), p.fields.clone())).unwrap_or_default();
                let held = self
                    .engine
                    .moves_on(&fields, &asks)
                    .or_else(|| self.engine.out_of_order(&transcript, &fields, &asks));
                if let Some(p) = self.pending_agent.as_mut().filter(|p| p.turn == turn) {
                    p.asks.clone_from(&asks);
                    if let Some(slot) = held {
                        tracing::info!(call = %self.info.call_sid, %slot, ?asks, "the reply moves on past an open question; its words are held");
                        p.hold_say = true;
                        p.held_for = Some(slot);
                    }
                }
            }
            Ev::AgentPhrase { turn, id } => {
                if self.pending_agent.as_ref().is_some_and(|p| p.turn == turn) {
                    self.agent_phrase(id);
                }
            }
            Ev::AgentDone { turn, result, rest, usage } => {
                let Some(p) = self.pending_agent.as_mut().filter(|p| p.turn == turn) else {
                    // A decision nobody waits for any more still cost its tokens.
                    if let Some(u) = &usage {
                        self.usage.add(u);
                    }
                    return;
                };
                if p.speculative {
                    p.done = Some((result, rest, usage));
                    return;
                }
                self.finish_agent(result, rest, usage);
            }
            Ev::TerminateNow => {}
            Ev::FinalOverdue { sent_at } => {
                if self.finalize_sent_at == Some(sent_at) {
                    // Every end of speech gets a transcript, empty for noise: none means the
                    // connection is up and dead. Without this, every answer was "the line is
                    // noisy" until the caller hung up.
                    tracing::warn!(call = %self.info.call_sid, "no transcript for the end of speech; reconnecting recognition");
                    self.finalize_sent_at = None;
                    if let Some(stt) = &self.stt {
                        let _ = stt.input.try_send(SttInput::Close);
                    }
                    self.on_stt(SttEvent::Closed);
                }
            }
        }
    }
}

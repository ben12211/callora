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
use callora_audio::playout::{OutFrame, PlayItem, Playout, PlayoutEvent, Source};
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

/// How long the caller's voice must go on over the agent before it stops, unless words come
/// first: a cough, a car horn or the TV cut the agent off mid-question in live calls. 450 ms
/// was a "הלו" over the greeting, which then stopped mid-sentence.
const BARGE_CONFIRM: Duration = Duration::from_millis(900);
/// The same over the greeting, when callers say "הלו", "כן?" as the line opens.
const BARGE_CONFIRM_GREETING: Duration = Duration::from_millis(1500);
/// The same during a read-back, where "כן", "אהה" are the caller listening.
const BARGE_CONFIRM_READ_BACK: Duration = Duration::from_millis(1200);
/// After speech with no words at all (noise the recognizer returned nothing for), how long
/// before the question is asked again: otherwise nothing is said until the caller speaks.
const NO_WORDS_WAIT: Duration = Duration::from_millis(2000);
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
    pub agent_speculate: bool,
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
    final_at: Option<Instant>,
    agent_first: Option<Instant>,
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
    /// The next agent request is told the caller's words overlap its last reply.
    overlap: bool,
    /// The route whose price list was asked ahead of the caller.
    priced: Option<(String, String)>,
    barge_in_started: Option<Instant>,
    /// The agent was cut off and no real utterance has followed yet.
    interrupted: bool,
    /// The caller's voice began over the agent, which goes on until words (or a long enough
    /// voice) show it is not noise.
    barge_pending: bool,
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
        let (playout, playout_task) = Playout::spawn(outbound, pl_tx, cfg.lead_frames);
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
            overlap: false,
            priced: None,
            barge_in_started: None,
            interrupted: false,
            barge_pending: false,
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
            cover_turn: None,
            phrase_words: Vec::new(),
            unfinished: None,
            unfinished_generation: 0,
            filler_turn: None,
            usage: Usage::default(),
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
        });
        metrics.calls_active.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        tracing::info!(call = %s.info.call_sid, ?ending, turns = s.engine.state.turns, "call ended");
        ending
    }

    fn on_event(&mut self, e: Ev) {
        match e {
            Ev::SttReady(Ok(session)) => {
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
                self.engine.state.second_hearing = text.filter(|t| !t.is_empty());
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
                if (rules_waiting || agent_waiting) && !self.agent_busy() && !filler_last_turn {
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
            Ev::TtsFirstChunk { elapsed } => self.services.metrics.tts_first_chunk.observe(elapsed.as_millis() as u64),
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

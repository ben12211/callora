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
use callora_core::config::MetaIntent;
use callora_core::customer::Customer;
use callora_core::engine::{Directive, Engine, HandoffSummary};
use callora_core::llm;
use callora_core::render::{SegmentOrigin, SpeechPlan, SpeechSegment};
use callora_core::speech::prepare_for_tts;
use callora_core::understanding::{fast_path, merge, Understanding};

use crate::metrics::Metrics;
use crate::ports::{
    ActionRunner, CallInfo, CallRecord, CallStore, LanguageModel, SpeechToText, SttEvent, SttInput, SttSession,
    Telephony, WhisperRegistry,
};

/// Up to this many words that nothing understood are treated as noise when the LLM cannot
/// be asked.
const SHORT_GARBAGE_WORDS: usize = 3;
/// How long an unfinished sentence waits for the caller to go on.
const UNFINISHED_WAIT: Duration = Duration::from_millis(1200);

/// Consecutive speech recognition reconnects (with no transcript in between) before the
/// call is handed off.
const MAX_STT_RECONNECTS: u32 = 4;

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
    /// A recognition session biased with a city's streets, opened beside the live one.
    SttFocused {
        city: String,
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
    AgentDone {
        turn: u64,
        result: anyhow::Result<Value>,
        rest: Option<String>,
    },
    Action {
        run_id: u64,
        action: String,
        input: Value,
        result: Result<Value, String>,
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
    /// The start of what may be a recorded phrase ("הכל טוב, תודה!" of "הכל טוב, תודה!
    /// איך אפשר לעזור?"), waiting for its next sentence so the whole clip plays.
    partial_phrase: Option<String>,
    /// The decision, when it finished while still speculative.
    done: Option<(anyhow::Result<Value>, Option<String>)>,
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
    barge_in_started: Option<Instant>,
    /// The agent was cut off and no real utterance has followed yet.
    interrupted: bool,
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
    stt_opening: Option<String>,
    stt_next: Option<(String, SttSession)>,
    pending_agent: Option<PendingAgent>,
    /// The recognizer's latest partial text for the utterance in progress.
    last_partial: String,
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
            barge_in_started: None,
            interrupted: false,
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
            clock: None,
            cover_turn: None,
            phrase_words: Vec::new(),
            unfinished: None,
            unfinished_generation: 0,
            filler_turn: None,
        };
        s.phrase_words = agent::phrases(&s.business).iter().map(|p| words(p)).collect();
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
        });
        metrics.calls_active.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        tracing::info!(call = %s.info.call_sid, ?ending, turns = s.engine.state.turns, "call ended");
        ending
    }

    // -----------------------------------------------------------------------------------
    // Caller audio

    fn on_audio(&mut self, frame: Bytes) {
        let vad_event = self.vad.push(&frame);
        self.keep_audio(&frame, vad_event.as_ref());
        match vad_event {
            Some(VadEvent::SpeechStarted) => {
                self.silence_generation += 1;
                if self.pending_agent.as_ref().is_some_and(|p| p.speculative) {
                    // The caller went on talking: the guess was about half a sentence.
                    if let Some(p) = self.pending_agent.take() {
                        p.task.abort();
                    }
                }
                if self.agent_busy() {
                    self.barge_in();
                }
            }
            Some(VadEvent::SpeechEnded) => {
                let now = Instant::now();
                self.speech_ended_at = Some(now);
                self.clock = Some(TurnClock {
                    speech_end: now,
                    final_at: None,
                    agent_first: None,
                    speculative_hit: false,
                    audio: "",
                });
                if let Some(stt) = &self.stt {
                    if stt.input.try_send(SttInput::Finalize).is_ok() {
                        self.finalize_sent_at = Some(now);
                    }
                }
                self.speculate();
            }
            None => {}
        }
        match &self.stt {
            Some(stt) => {
                if stt.input.try_send(SttInput::Audio(frame)).is_err() {
                    tracing::warn!(call = %self.info.call_sid, "stt input is full; dropping a frame");
                }
            }
            None => {
                self.stt_backlog.push_back(frame);
                while self.stt_backlog.len() > self.cfg.stt_buffer_frames {
                    self.stt_backlog.pop_front();
                }
            }
        }
    }

    /// The caller's words as audio, for a second hearing: from 300 ms before speech starts
    /// to its end (a transcript may join a few of these).
    fn keep_audio(&mut self, frame: &Bytes, event: Option<&VadEvent>) {
        const PREROLL_FRAMES: usize = 15;
        const MAX_BYTES: usize = 8000 * 20;
        match event {
            Some(VadEvent::SpeechStarted) => {
                self.utterance = self.preroll.iter().flat_map(|f| f.iter().copied()).collect();
                self.utterance.extend_from_slice(frame);
            }
            Some(VadEvent::SpeechEnded) => {
                self.utterance.extend_from_slice(frame);
                let finished = std::mem::take(&mut self.utterance);
                // Pieces of one sentence split by a pause stay together.
                if self.second_pending.is_none() && self.finalize_sent_at.is_none() {
                    self.last_utterance.clear();
                }
                self.last_utterance.extend(finished);
                if self.last_utterance.len() > MAX_BYTES {
                    let cut = self.last_utterance.len() - MAX_BYTES;
                    self.last_utterance.drain(..cut);
                }
            }
            None if self.vad.is_speaking() => {
                if self.utterance.len() < MAX_BYTES {
                    self.utterance.extend_from_slice(frame);
                }
            }
            _ => {}
        }
        self.preroll.push_back(frame.clone());
        while self.preroll.len() > PREROLL_FRAMES {
            self.preroll.pop_front();
        }
    }

    /// Keyterms for a second hearing of this turn, when it is worth one: the caller is
    /// giving a street (every street of the city) or a city (the towns). Elsewhere the
    /// stream is good enough and waiting would only slow the call.
    fn second_hearing_terms(&self) -> Option<Vec<String>> {
        let g = self.services.gazetteer.as_ref()?;
        self.services.second_hearing.as_ref()?;
        if self.last_utterance.len() < 8000 / 4 {
            return None;
        }
        let mut terms = if let Some(city) = self.engine.street_focus() {
            let mut t = vec![city.clone()];
            t.extend(g.street_keyterms(&city, 950));
            t
        } else if self.engine.awaiting_city() {
            g.town_names(20)
        } else {
            return None;
        };
        terms.extend(self.business.stt_keyterms());
        Some(terms)
    }

    fn barge_in(&mut self) {
        self.barge_in_started = Some(Instant::now());
        self.interrupted = true;
        self.playout.cancel();
        self.services.metrics.barge_ins_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::debug!(call = %self.info.call_sid, rms = self.vad.last_rms, "barge-in: caller talked over the agent");
    }

    // -----------------------------------------------------------------------------------
    // Transcripts and understanding

    fn on_stt(&mut self, event: SttEvent) {
        match event {
            SttEvent::Partial(text) => self.last_partial = text,
            SttEvent::Final(text) => {
                self.stt_reconnects = 0;
                self.on_final(text);
                self.swap_stt_if_ready();
            }
            SttEvent::Error(e) => tracing::warn!(call = %self.info.call_sid, error = %e, "stt error"),
            SttEvent::Closed => {
                self.stt = None;
                self.stt_reconnects += 1;
                if self.stt_reconnects > MAX_STT_RECONNECTS {
                    // A session the service keeps closing (a rejected request, an outage)
                    // would otherwise reconnect forever while the caller talks to no one.
                    tracing::error!(call = %self.info.call_sid, "speech recognition keeps closing; giving up");
                    let d = self.engine.force_handoff("stt_unavailable");
                    self.execute(d);
                    return;
                }
                tracing::warn!(call = %self.info.call_sid, attempt = self.stt_reconnects, "stt closed; reconnecting");
                let stt = self.services.stt.clone();
                let language = self.business.config.language.clone();
                let keyterms = self.stt_keyterms(self.stt_city.clone().as_deref());
                let tx = self.events.clone();
                let delay = Duration::from_millis(200 * u64::from(self.stt_reconnects));
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    let _ = tx.send(Ev::SttReady(stt.open(&language, &keyterms).await));
                });
            }
        }
    }

    fn on_final(&mut self, text: String) {
        if let Some(t) = self.finalize_sent_at.take() {
            self.services.metrics.stt_final.observe(t.elapsed().as_millis() as u64);
        }
        let text = text.trim().to_string();
        if text.is_empty() {
            return;
        }
        if self.pending_llm.is_none() {
            let (u, _) = fast_path(&self.business, &self.engine.context(), &text);
            if u.noise {
                return self.on_noise(&text);
            }
        }
        self.interrupted = false;
        self.silence_generation += 1;
        if self.speech_ended_at.is_none() {
            self.speech_ended_at = Some(Instant::now());
        }
        // Some recognizers only send finals: a transcript while the agent talks is also a
        // barge-in (the VAD may have missed a quiet caller).
        if self.agent_busy() {
            self.barge_in();
        }
        if let Some(c) = &mut self.clock {
            c.final_at.get_or_insert_with(Instant::now);
        }
        // The recognizer marks a sentence the caller broke off ("ואני רוצה להגיע ל...",
        // "מתל-ב-ב-"): answering it talks over them. Wait for the rest; a short pause
        // later, answer what there is.
        let text = match self.unfinished.take() {
            Some(start) => format!("{start} {text}"),
            None => text,
        };
        if is_unfinished(&text) {
            tracing::info!(call = %self.info.call_sid, caller = %text, "unfinished sentence; waiting for the rest");
            self.unfinished = Some(text);
            self.unfinished_generation += 1;
            let generation = self.unfinished_generation;
            let tx = self.events.clone();
            tokio::spawn(async move {
                tokio::time::sleep(UNFINISHED_WAIT).await;
                let _ = tx.send(Ev::UnfinishedDue { generation });
            });
            return;
        }
        if self.agent_mode() {
            return self.on_final_agent(text);
        }
        // A new sentence while the LLM is still thinking about the previous one: they are
        // one utterance.
        let transcript = match self.pending_llm.take() {
            Some(p) => {
                p.task.abort();
                format!("{} {text}", p.transcript)
            }
            None => text,
        };
        self.services.metrics.turns_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::info!(call = %self.info.call_sid, caller = %transcript, "caller");

        let (fast, needs_llm) = fast_path(&self.business, &self.engine.context(), &transcript);
        match (&self.services.llm, needs_llm) {
            (Some(model), true) => {
                self.turn += 1;
                let turn = self.turn;
                let request =
                    llm::build_request(&self.business, &self.engine.context(), &self.engine.state, &transcript);
                let timeout = Duration::from_millis(self.business.config.understanding.llm_timeout_ms);
                let model = model.clone();
                let tx = self.events.clone();
                let task = tokio::spawn(async move {
                    let started = Instant::now();
                    let result = match tokio::time::timeout(timeout, model.extract(&request)).await {
                        Ok(r) => r,
                        Err(_) => Err(anyhow::anyhow!("timed out after {timeout:?}")),
                    };
                    let _ = tx.send(Ev::Llm { turn, result, elapsed: started.elapsed() });
                });
                if self.business.config.understanding.thinking_filler.is_some() {
                    let tx = self.events.clone();
                    let after = Duration::from_millis(self.business.config.understanding.filler_after_ms);
                    tokio::spawn(async move {
                        tokio::time::sleep(after).await;
                        let _ = tx.send(Ev::FillerDue { turn });
                    });
                }
                self.services.metrics.llm_calls_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.pending_llm = Some(PendingLlm { turn, transcript, fast, task });
            }
            _ => self.understood(fast),
        }
    }

    /// A transcript of nothing but filler words, which recognizers invent on line noise
    /// ("תודה."). It must not talk over the agent; if noise already cut the agent off,
    /// the reply is said again.
    fn on_noise(&mut self, text: &str) {
        tracing::info!(call = %self.info.call_sid, caller = %text, "ignored as noise");
        if self.agent_busy() {
            return;
        }
        if std::mem::take(&mut self.interrupted) {
            let directives = self.engine.replay_last();
            self.execute(directives);
        } else {
            self.arm_silence();
        }
    }

    // -----------------------------------------------------------------------------------
    // The conversation agent

    fn agent_mode(&self) -> bool {
        self.services.agent.is_some() && self.business.config.agent.is_some()
    }

    /// Utterances with one certain meaning skip the agent: "רגע", "מה?", "לא שמעתי", and a
    /// plain yes to a read-back (so a booking goes out the moment the caller confirms it).
    fn fast_lane(&self, u: &Understanding, needs_llm: bool) -> bool {
        if needs_llm || u.intent.is_some() || !u.slots.is_empty() {
            return false;
        }
        match u.meta {
            Some(m) => matches!(
                m,
                MetaIntent::Wait
                    | MetaIntent::RepeatLast
                    | MetaIntent::DidNotUnderstand
                    | MetaIntent::SpeakSlower
                    | MetaIntent::SpeakLouder
            ),
            None => {
                u.affirm == Some(true)
                    && u.coverage >= 0.99
                    && self
                        .engine
                        .state
                        .run
                        .as_ref()
                        .is_some_and(|r| r.step == callora_core::state::Step::AwaitingConfirmation)
            }
        }
    }

    /// At the end of speech, start the agent on the recognizer's partial text instead of
    /// waiting ~230 ms for the final transcript. Its speech is held until the final
    /// transcript confirms the words; a different final starts over.
    fn speculate(&mut self) {
        if !self.cfg.agent_speculate || !self.agent_mode() || self.pending_agent.is_some() || self.pending_llm.is_some()
        {
            return;
        }
        let text = self.last_partial.trim().to_string();
        if text.is_empty() {
            return;
        }
        let (u, needs_llm) = fast_path(&self.business, &self.engine.context(), &text);
        if u.noise || self.fast_lane(&u, needs_llm) {
            return;
        }
        self.start_agent(text, true);
    }

    fn on_final_agent(&mut self, text: String) {
        self.last_partial.clear();
        let mut transcript = text;
        if let Some(p) = self.pending_agent.take() {
            if p.speculative && same_words(&p.transcript, &transcript) {
                tracing::info!(call = %self.info.call_sid, caller = %transcript, "caller");
                self.services.metrics.turns_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if let Some(c) = &mut self.clock {
                    c.speculative_hit = true;
                }
                self.pending_agent = Some(p);
                return self.adopt_speculation();
            }
            p.task.abort();
            // A new sentence while the agent was still thinking about the previous one:
            // they are one utterance.
            if !p.speculative && p.spoken.is_empty() {
                transcript = format!("{} {transcript}", p.transcript);
            }
        }
        self.services.metrics.turns_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::info!(call = %self.info.call_sid, caller = %transcript, "caller");
        let (fast, needs_llm) = fast_path(&self.business, &self.engine.context(), &transcript);
        if self.fast_lane(&fast, needs_llm) {
            return self.understood(fast);
        }
        // A city or street expected: hear the words once more, with every name expected as a
        // hint, before the agent decides ("זה ביתר" came back "זה יותר", "אהרונוביץ" as
        // "עונה מ-32"). The agent gets both.
        if let Some((_, earlier)) = self.second_pending.take() {
            transcript = format!("{earlier} {transcript}");
        }
        if let (Some(terms), Some(t)) = (self.second_hearing_terms(), self.services.second_hearing.clone()) {
            self.second_ids += 1;
            let id = self.second_ids;
            let audio = self.last_utterance.clone();
            let language = self.business.config.language.clone();
            let tx = self.events.clone();
            let started = Instant::now();
            let call = self.info.call_sid.clone();
            tokio::spawn(async move {
                let heard =
                    tokio::time::timeout(Duration::from_millis(1500), t.transcribe(&audio, &language, &terms)).await;
                let text = match heard {
                    Ok(Ok(text)) => Some(text),
                    Ok(Err(e)) => {
                        tracing::warn!(%call, error = %e, "second hearing failed");
                        None
                    }
                    Err(_) => {
                        tracing::warn!(%call, "second hearing timed out");
                        None
                    }
                };
                tracing::info!(%call, second = text.as_deref().unwrap_or(""), ms = started.elapsed().as_millis() as u64, "second hearing");
                let _ = tx.send(Ev::SecondHearing { id, text });
            });
            self.second_pending = Some((id, transcript));
            return;
        }
        self.engine.state.second_hearing = None;
        self.start_agent(transcript, false);
    }

    fn adopt_speculation(&mut self) {
        let Some(p) = self.pending_agent.as_mut() else { return };
        p.speculative = false;
        let held = std::mem::take(&mut p.held);
        let done = p.done.take();
        for sentence in held {
            self.agent_sentence(sentence);
        }
        if let Some((result, rest)) = done {
            self.finish_agent(result, rest);
        }
    }

    fn start_agent(&mut self, transcript: String, speculative: bool) {
        let (Some(model), Some(cfg)) = (self.services.agent.clone(), self.business.config.agent.clone()) else {
            return;
        };
        self.turn += 1;
        let turn = self.turn;
        let request = agent::build_request(&self.business, &self.engine.state, &transcript);
        let timeout = Duration::from_millis(cfg.timeout_ms);
        let tx = self.events.clone();
        let task = tokio::spawn(async move {
            let says = tx.clone();
            let decide = async move {
                let mut stream = model.stream(&request).await?;
                let mut say = SayStream::default();
                let mut reply = String::new();
                while let Some(delta) = stream.next().await {
                    let delta = delta?;
                    reply.push_str(&delta);
                    let sentences = say.push(&delta);
                    // A read-back or a submit: the engine speaks the words with what follows.
                    if matches!(say.action(), Some(AgentAction::ReadBack | AgentAction::Submit | AgentAction::EndCall))
                    {
                        continue;
                    }
                    for sentence in sentences {
                        let _ = says.send(Ev::AgentSay { turn, sentence });
                    }
                }
                let value: Value = serde_json::from_str(&reply)
                    .map_err(|e| anyhow::anyhow!("the agent's reply is not JSON ({e}): {reply}"))?;
                let held =
                    matches!(say.action(), Some(AgentAction::ReadBack | AgentAction::Submit | AgentAction::EndCall));
                let rest = say.rest().filter(|_| !held);
                anyhow::Ok((value, rest))
            };
            let (result, rest) = match tokio::time::timeout(timeout, decide).await {
                Ok(Ok((value, rest))) => (Ok(value), rest),
                Ok(Err(e)) => (Err(e), None),
                Err(_) => (Err(anyhow::anyhow!("timed out after {timeout:?}")), None),
            };
            let _ = tx.send(Ev::AgentDone { turn, result, rest });
        });
        if cfg.thinking_filler.is_some() {
            let tx = self.events.clone();
            let after = Duration::from_millis(cfg.filler_after_ms);
            tokio::spawn(async move {
                tokio::time::sleep(after).await;
                let _ = tx.send(Ev::FillerDue { turn });
            });
        }
        self.services.metrics.llm_calls_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.pending_agent = Some(PendingAgent {
            turn,
            transcript,
            task,
            started: Instant::now(),
            speculative,
            spoken: Vec::new(),
            held: Vec::new(),
            partial_phrase: None,
            done: None,
        });
    }

    /// One sentence of the agent's reply, as it streams in. A sentence that begins one of
    /// the recorded phrases waits for the next one, so the phrase plays as its clip instead
    /// of half of it going through live TTS.
    fn agent_sentence(&mut self, sentence: String) {
        let Some(p) = self.pending_agent.as_mut() else { return };
        if let Some(start) = p.partial_phrase.take() {
            let joined = format!("{start} {sentence}");
            if self.continues_a_phrase(&joined) || self.library_has(&joined) {
                return self.agent_sentence_text(joined);
            }
            // "הכל טוב, תודה!" then "מאיפה אוספים אותך?": two recordings, not one live TTS.
            // The start plays now (it would only wait again), then the new sentence.
            if let Some(p) = self.pending_agent.as_mut() {
                p.spoken.push(start.clone());
            }
            self.say_now(&start);
            return self.agent_sentence_text(sentence);
        }
        self.agent_sentence_text(sentence);
    }

    fn agent_sentence_text(&mut self, text: String) {
        let Some(p) = self.pending_agent.as_mut() else { return };
        let is_prefix = {
            let w = words(&text);
            self.phrase_words.iter().any(|ph| ph.len() > w.len() && ph.starts_with(&w))
        };
        if is_prefix {
            p.partial_phrase = Some(text);
            return;
        }
        p.spoken.push(text.clone());
        self.say_now(&text);
    }

    fn continues_a_phrase(&self, text: &str) -> bool {
        let w = words(text);
        self.phrase_words.iter().any(|ph| ph.len() > w.len() && ph.starts_with(&w))
    }

    fn library_has(&self, text: &str) -> bool {
        let delivery = self.engine.state.delivery.clone().unwrap_or_else(|| "normal".into());
        self.library.get_loose(&delivery, text).is_some()
    }

    /// Play one sentence of the agent's reply now.
    fn say_now(&mut self, sentence: &str) {
        let delivery = self.engine.state.delivery.clone().unwrap_or_else(|| "normal".into());
        let recorded = self.library.get_loose(&delivery, sentence).is_some();
        if let Some(c) = &mut self.clock {
            c.agent_first.get_or_insert_with(Instant::now);
            if c.audio.is_empty() {
                c.audio = if recorded { "recorded" } else { "live tts" };
            }
        }
        self.services.store.record(CallRecord::Turn {
            call_id: self.info.call_id,
            speaker: "agent".into(),
            text: sentence.to_string(),
            detail: json!({ "responses": ["agent"] }),
        });
        tracing::info!(call = %self.info.call_sid, agent = %sentence, "agent");
        self.speak(SpeechPlan::free(sentence, &delivery, self.engine.state.gain_db));
    }

    fn finish_agent(&mut self, result: anyhow::Result<Value>, rest: Option<String>) {
        let Some(mut p) = self.pending_agent.take() else { return };
        self.services.metrics.llm_latency.observe(p.started.elapsed().as_millis() as u64);
        match result {
            Ok(reply) => {
                // Whatever is left, with the start of a phrase that was waiting for it.
                let tail = [p.partial_phrase.take(), rest].into_iter().flatten().collect::<Vec<_>>().join(" ");
                if !tail.is_empty() {
                    p.spoken.push(tail.clone());
                    self.say_now(&tail);
                }
                let decision = agent::parse(&self.business, &reply);
                tracing::info!(call = %self.info.call_sid, action = ?decision.action, task = ?decision.task, fields = ?decision.fields, "agent decision");
                let directives = self.engine.on_agent_turn(&p.transcript, decision, &p.spoken.join(" "));
                self.execute(directives);
            }
            Err(error) => {
                self.services.metrics.llm_failures_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(call = %self.info.call_sid, error = %format!("{error:#}"), "the agent failed; the rules take this turn");
                let (mut fast, _) = fast_path(&self.business, &self.engine.context(), &p.transcript);
                if fast.is_empty() && fast.transcript.split_whitespace().count() <= SHORT_GARBAGE_WORDS {
                    fast.noise = true;
                }
                if fast.noise {
                    return self.on_noise(&p.transcript);
                }
                self.understood(fast);
            }
        }
    }

    fn understood(&mut self, u: Understanding) {
        self.services.store.record(CallRecord::Turn {
            call_id: self.info.call_id,
            speaker: "caller".into(),
            text: u.transcript.clone(),
            detail: serde_json::to_value(&u).unwrap_or(Value::Null),
        });
        let directives = self.engine.on_utterance(u);
        self.execute(directives);
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
                if self.stt_opening.as_deref() == Some(city.as_str()) {
                    self.stt_opening = None;
                }
                match result {
                    Ok(session) => {
                        self.stt_next = Some((city, session));
                        self.swap_stt_if_ready();
                    }
                    Err(error) => {
                        tracing::warn!(call = %self.info.call_sid, %city, %error, "city recognition hints unavailable")
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
                    && !self.agent_busy()
                    && self.pending_llm.is_none()
                    && self.actions_in_flight == 0
                    && self.after_speech.is_none()
                {
                    let d = self.engine.on_silence();
                    self.execute(d);
                }
            }
            Ev::TtsFirstChunk { elapsed } => self.services.metrics.tts_first_chunk.observe(elapsed.as_millis() as u64),
            Ev::UnfinishedDue { generation } => {
                if generation == self.unfinished_generation && !self.vad.is_speaking() {
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
            Ev::AgentDone { turn, result, rest } => {
                let Some(p) = self.pending_agent.as_mut().filter(|p| p.turn == turn) else { return };
                if p.speculative {
                    p.done = Some((result, rest));
                    return;
                }
                self.finish_agent(result, rest);
            }
            Ev::TerminateNow => {}
        }
    }

    // -----------------------------------------------------------------------------------
    // Directives

    /// Recognition hints: the city's streets when the call waits for one, then the business's
    /// own words (the recognizer keeps the first 50).
    fn stt_keyterms(&self, city: Option<&str>) -> Vec<String> {
        let mut terms = Vec::new();
        if let (Some(city), Some(g)) = (city, &self.services.gazetteer) {
            terms.push(city.to_string());
            terms.extend(g.street_keyterms(city, 38));
        }
        terms.extend(self.business.stt_keyterms());
        let mut seen = std::collections::HashSet::new();
        terms.retain(|t| seen.insert(t.clone()));
        terms
    }

    /// "בני ברק" given, its street next: open a session biased with its streets beside the
    /// live one. Recognition cannot be re-biased mid-session, and "אהרונוביץ" in an Ashkenazi
    /// accent came back as "עונה מ-32" without the hint.
    fn focus_stt(&mut self) {
        let Some(city) = self.engine.street_focus() else { return };
        if self.stt_city.as_ref() == Some(&city)
            || self.stt_opening.as_ref() == Some(&city)
            || self.stt_next.as_ref().is_some_and(|(c, _)| *c == city)
            || self.services.gazetteer.is_none()
        {
            return;
        }
        self.stt_opening = Some(city.clone());
        let stt = self.services.stt.clone();
        let language = self.business.config.language.clone();
        let keyterms = self.stt_keyterms(Some(&city));
        let tx = self.events.clone();
        tokio::spawn(async move {
            let result = stt.open(&language, &keyterms).await;
            let _ = tx.send(Ev::SttFocused { city, result });
        });
    }

    /// The biased session takes over between utterances only: never while the caller is
    /// talking or a transcript is still due from the live session.
    fn swap_stt_if_ready(&mut self) {
        if self.stt_next.is_none() || self.vad.is_speaking() || self.finalize_sent_at.is_some() {
            return;
        }
        let Some((city, session)) = self.stt_next.take() else { return };
        if let Some(old) = self.stt.take() {
            let _ = old.input.try_send(SttInput::Close);
        }
        tracing::info!(call = %self.info.call_sid, %city, "recognition biased with the city's streets");
        self.stt = Some(session);
        self.stt_city = Some(city);
    }

    fn execute(&mut self, directives: Vec<Directive>) {
        self.focus_stt();
        for d in directives {
            match d {
                Directive::Speak { plan, .. } => {
                    self.services.store.record(CallRecord::Turn {
                        call_id: self.info.call_id,
                        speaker: "agent".into(),
                        text: plan.text(),
                        detail: json!({ "responses": plan.response_ids() }),
                    });
                    tracing::info!(call = %self.info.call_sid, agent = %plan.text(), "agent");
                    self.speak(plan);
                }
                Directive::RunAction { run_id, action, input } => {
                    self.actions_in_flight += 1;
                    let runner = self.services.actions.clone();
                    let business = self.business.clone();
                    let info = self.info.clone();
                    let tx = self.events.clone();
                    tokio::spawn(async move {
                        let started = Instant::now();
                        let result = runner.run(&business, &action, input.clone(), &info).await;
                        let _ = tx.send(Ev::Action { run_id, action, input, result, elapsed: started.elapsed() });
                    });
                }
                Directive::Handoff { summary } => {
                    self.services
                        .store
                        .record(CallRecord::Handoff { call_id: self.info.call_id, summary: summary.clone() });
                    self.after_speech = Some(AfterSpeech::Handoff(summary));
                }
                Directive::Hangup => self.after_speech = Some(AfterSpeech::Hangup),
            }
        }
        // Nothing queued to say: the terminal action happens right away; otherwise it waits
        // for the playout to go idle, i.e. for the goodbye to have been heard.
        if !self.agent_busy() && self.after_speech.is_some() {
            let _ = self.events.send(Ev::TerminateNow);
        }
    }

    /// Queue a plan: each segment from the voice library when it is there, otherwise from
    /// dynamic TTS (streamed, and cached for next time).
    fn speak(&mut self, plan: SpeechPlan) {
        // A reply that opens with live TTS would start with a second of silence.
        if !self.agent_busy() && plan.segments.first().is_some_and(|s| self.needs_live_tts(s)) {
            self.cover_live_tts(plan.gain_db);
        }
        for whole in &plan.segments {
            // A long sentence for live TTS goes out as short pieces synthesized side by side:
            // eleven_v3 took 6 to 31 s on a whole read-back, ~1 s on each short piece.
            let pieces: Vec<SpeechSegment> = if self.library.get_loose(&whole.delivery, &whole.text).is_some() {
                vec![whole.clone()]
            } else {
                split_for_tts(&whole.text).into_iter().map(|text| SpeechSegment { text, ..whole.clone() }).collect()
            };
            for seg in &pieces {
                let id = self.next_item;
                self.next_item += 1;
                if let Some(clip) = self.library.get_loose(&seg.delivery, &seg.text) {
                    self.services.metrics.segment(if seg.origin == SegmentOrigin::Template {
                        "template"
                    } else {
                        "cached"
                    });
                    self.enqueue(PlayItem { id, source: Source::Clip(clip), gain_db: plan.gain_db });
                    continue;
                }
                let (Some(tts), Some(request)) = (self.services.tts.clone(), self.tts_request(seg)) else {
                    tracing::error!(call = %self.info.call_sid, text = %seg.text, "not in the voice library and no TTS configured; segment skipped");
                    continue;
                };
                let key = request.cache_key();
                if let Some(audio) = self.services.tts_cache.get(&key) {
                    self.services.metrics.segment("tts_cached");
                    self.enqueue(PlayItem { id, source: Source::Clip(audio), gain_db: plan.gain_db });
                    continue;
                }
                self.services.metrics.segment("tts");
                let (tx, rx) = mpsc::channel(64);
                self.enqueue(PlayItem { id, source: Source::Stream(rx), gain_db: plan.gain_db });
                spawn_tts(tts, request, key, tx, self.services.tts_cache.clone(), self.events.clone());
            }
        }
    }

    fn tts_request(&self, seg: &SpeechSegment) -> Option<TtsRequest> {
        let c = &self.business.config;
        Some(TtsRequest {
            text: prepare_for_tts(&seg.text, &c.language, self.business.pronouncer_for(self.engine.state.address_form)),
            voice_id: self.business.voice_id.clone()?,
            model: self.cfg.dynamic_model.clone().unwrap_or_else(|| c.voice.dynamic_model.clone()),
            settings: c.voice.settings_for(&seg.delivery),
            language: c.language.clone(),
        })
    }

    /// Neither pre-generated nor already synthesized this process: it will take a while.
    fn needs_live_tts(&self, seg: &SpeechSegment) -> bool {
        self.services.tts.is_some()
            && self.library.get_loose(&seg.delivery, &seg.text).is_none()
            && self.tts_request(seg).is_some_and(|r| self.services.tts_cache.get(&r.cache_key()).is_none())
    }

    /// The business's short opener, from the library only (it must never need TTS itself).
    fn cover_live_tts(&mut self, gain_db: f32) {
        let turn = self.engine.state.turns;
        if self.cover_turn.is_some_and(|t| t + 1 >= turn) || self.filler_turn.is_some_and(|t| t + 1 >= turn) {
            return;
        }
        let Some(id) = self.business.config.voice.dynamic_cover.clone() else { return };
        self.cover_turn = Some(turn);
        let Some(plan) = self.engine.render_response(&id) else { return };
        for seg in &plan.segments {
            if let Some(clip) = self.library.get(&seg.delivery, &seg.text) {
                let id = self.next_item;
                self.next_item += 1;
                self.services.metrics.segment("cover");
                self.enqueue(PlayItem { id, source: Source::Clip(clip), gain_db });
            }
        }
    }

    // -----------------------------------------------------------------------------------
    // Playout

    async fn on_playout(&mut self, e: PlayoutEvent) -> Option<Ending> {
        match e {
            PlayoutEvent::Started { at, .. } => {
                self.speaking = true;
                if let Some(t) = self.speech_ended_at.take() {
                    self.services.metrics.response_latency.observe(at.saturating_duration_since(t).as_millis() as u64);
                }
                if let Some(c) = self.clock.take_if(|c| c.final_at.is_some()) {
                    let ms = |a: Instant, b: Instant| b.saturating_duration_since(a).as_millis() as u64;
                    let final_at = c.final_at.unwrap_or(c.speech_end);
                    tracing::info!(
                        call = %self.info.call_sid,
                        stt_ms = ms(c.speech_end, final_at),
                        agent_first_words_ms = c.agent_first.map(|a| ms(c.speech_end, a)),
                        speculative_hit = c.speculative_hit,
                        audio = c.audio,
                        reply_ms = ms(c.speech_end, at),
                        "turn timing (from the end of the caller's speech)"
                    );
                }
            }
            PlayoutEvent::Cancelled { ids } => {
                self.queued_items = self.queued_items.saturating_sub(ids.len());
                if let Some(t) = self.barge_in_started.take() {
                    let detect = self.cfg.vad.trigger_ms;
                    self.services.metrics.barge_in_latency.observe(detect + t.elapsed().as_millis() as u64);
                }
            }
            PlayoutEvent::Idle => {
                self.speaking = false;
                if let Some(after) = self.after_speech.take() {
                    return Some(self.terminate(after).await);
                }
                self.arm_silence();
            }
            PlayoutEvent::Finished { .. } => self.queued_items = self.queued_items.saturating_sub(1),
            PlayoutEvent::Failed { .. } => {}
        }
        None
    }

    fn agent_busy(&self) -> bool {
        self.queued_items > 0
    }

    fn enqueue(&mut self, item: PlayItem) {
        self.queued_items += 1;
        self.playout.enqueue(item);
    }

    fn arm_silence(&mut self) {
        self.silence_generation += 1;
        let generation = self.silence_generation;
        let after = Duration::from_millis(self.business.config.silence.reprompt_after_ms);
        let tx = self.events.clone();
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            let _ = tx.send(Ev::Silence { generation });
        });
    }

    async fn terminate(&mut self, after: AfterSpeech) -> Ending {
        match after {
            AfterSpeech::Hangup => {
                if let Err(e) = self.services.telephony.hangup(&self.info.call_sid).await {
                    tracing::error!(call = %self.info.call_sid, error = %e, "hangup failed");
                }
                Ending::AgentHungUp
            }
            AfterSpeech::Handoff(summary) => {
                self.services.metrics.handoffs_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(number) = self.business.handoff_number.clone() else {
                    let _ = self.services.telephony.hangup(&self.info.call_sid).await;
                    return Ending::AgentHungUp;
                };
                let whisper = self.services.whisper.register(&self.info, &summary);
                if let Err(e) = self.services.telephony.transfer(&self.info.call_sid, &number, whisper.as_deref()).await
                {
                    tracing::error!(call = %self.info.call_sid, error = %e, "transfer failed; hanging up");
                    let _ = self.services.telephony.hangup(&self.info.call_sid).await;
                    return Ending::AgentHungUp;
                }
                Ending::HandedOff
            }
        }
    }
}

/// The same words, ignoring punctuation, spacing and case: a partial transcript that
/// already says what the final one says.
/// Pieces of a sentence short enough for fast live TTS: split after commas and sentence
/// marks, then merged back so no piece is a lone word ("סגור.") next to a short neighbour.
fn split_for_tts(text: &str) -> Vec<String> {
    const SHORT: usize = 45;
    if text.chars().count() <= SHORT {
        return vec![text.to_string()];
    }
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        current.push(ch);
        if matches!(ch, ',' | '.' | '?' | '!') {
            parts.push(std::mem::take(&mut current));
        }
    }
    if !current.trim().is_empty() {
        parts.push(current);
    }
    let mut out: Vec<String> = Vec::new();
    for p in parts.into_iter().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()) {
        match out.last_mut() {
            Some(last) if last.chars().count() + p.chars().count() < 20 => {
                last.push(' ');
                last.push_str(&p);
            }
            _ => out.push(p),
        }
    }
    out
}

/// The recognizer's marks for a sentence the caller broke off: a trailing "..." or "-".
fn is_unfinished(text: &str) -> bool {
    let t = text.trim_end();
    t.ends_with("...") || t.ends_with('…') || t.ends_with('-')
}

fn same_words(a: &str, b: &str) -> bool {
    words(a) == words(b)
}

/// The words of a sentence, without punctuation or case.
fn words(s: &str) -> Vec<String> {
    s.split_whitespace()
        .map(|w| w.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase())
        .filter(|w| !w.is_empty())
        .collect()
}

fn spawn_tts(
    tts: Arc<dyn Synthesizer>,
    request: TtsRequest,
    key: String,
    tx: mpsc::Sender<anyhow::Result<Bytes>>,
    cache: TtsCache,
    events: mpsc::UnboundedSender<Ev>,
) {
    use futures::StreamExt;
    tokio::spawn(async move {
        let started = Instant::now();
        let mut stream = match tts.synthesize(request).await {
            Ok(s) => s,
            Err(e) => {
                let _ = tx.send(Err(e)).await;
                return;
            }
        };
        let mut all = Vec::new();
        let mut first = true;
        let mut listener = true;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    if first {
                        first = false;
                        let _ = events.send(Ev::TtsFirstChunk { elapsed: started.elapsed() });
                    }
                    all.extend_from_slice(&bytes);
                    // Keep synthesizing after a barge-in: the finished audio goes to the
                    // cache, and "what?" usually asks for exactly this sentence again.
                    if listener && tx.send(Ok(bytes)).await.is_err() {
                        listener = false;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            }
        }
        if !all.is_empty() {
            cache.put(key, Bytes::from(all));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::is_unfinished;

    #[test]
    fn long_live_sentences_are_split_into_short_pieces() {
        let read_back = "שלושה נוסעים מרבן יוחנן בן זכאי 45, אלעד לאהרונוביץ ראובן 42, בני ברק, עכשיו. לשלוח?";
        let pieces = super::split_for_tts(read_back);
        assert_eq!(pieces.join(" "), read_back, "nothing is lost");
        assert!(pieces.len() >= 3, "{pieces:?}");
        assert!(pieces.iter().all(|p| p.chars().count() <= 40), "{pieces:?}");
        assert_eq!(super::split_for_tts("לאיזה רחוב בבני ברק?"), vec!["לאיזה רחוב בבני ברק?"]);
    }

    #[test]
    fn broken_off_sentences_are_recognised() {
        for t in ["ואני רוצה להגיע ל...", "יעני, מתל-ב-ב-ב-ב-", "אני נוסע ל… "] {
            assert!(is_unfinished(t), "{t}");
        }
        for t in ["לתל אביב.", "מה המצב?", "3-4 נוסעים"] {
            assert!(!is_unfinished(t), "{t}");
        }
    }
}

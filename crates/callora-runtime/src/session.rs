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
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use callora_audio::library::VoiceLibrary;
use callora_audio::playout::{OutFrame, PlayItem, Playout, PlayoutEvent, Source};
use callora_audio::tts::{Synthesizer, TtsCache, TtsRequest};
use callora_audio::vad::{Vad, VadConfig, VadEvent};
use callora_core::business::Business;
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
        }
    }
}

#[derive(Clone)]
pub struct Services {
    pub stt: Arc<dyn SpeechToText>,
    pub llm: Option<Arc<dyn LanguageModel>>,
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
    Llm {
        turn: u64,
        result: anyhow::Result<Value>,
        elapsed: Duration,
    },
    FillerDue {
        turn: u64,
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
        let mut s = Session {
            engine: Engine::new(business.clone(), seed),
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
            filler_turn: None,
        };
        s.services.store.record(CallRecord::Started { info: s.info.clone() });

        // STT connects in the background; the greeting does not wait for it.
        {
            let stt = s.services.stt.clone();
            let language = s.business.config.language.clone();
            let keyterms = s.business.stt_keyterms();
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
        playout_task.abort();
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
        match self.vad.push(&frame) {
            Some(VadEvent::SpeechStarted) => {
                self.silence_generation += 1;
                if self.agent_busy() {
                    self.barge_in();
                }
            }
            Some(VadEvent::SpeechEnded) => {
                self.speech_ended_at = Some(Instant::now());
                if let Some(stt) = &self.stt {
                    if stt.input.try_send(SttInput::Finalize).is_ok() {
                        self.finalize_sent_at = Some(Instant::now());
                    }
                }
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
            SttEvent::Partial(_) => {}
            SttEvent::Final(text) => {
                self.stt_reconnects = 0;
                self.on_final(text)
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
                let keyterms = self.business.stt_keyterms();
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
                if self.pending_llm.as_ref().map(|p| p.turn) == Some(turn) && !self.agent_busy() && !filler_last_turn {
                    if let Some(id) = self.business.config.understanding.thinking_filler.clone() {
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
            Ev::TerminateNow => {}
        }
    }

    // -----------------------------------------------------------------------------------
    // Directives

    fn execute(&mut self, directives: Vec<Directive>) {
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
        for seg in &plan.segments {
            let id = self.next_item;
            self.next_item += 1;
            if let Some(clip) = self.library.get(&seg.delivery, &seg.text) {
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

    fn tts_request(&self, seg: &SpeechSegment) -> Option<TtsRequest> {
        let c = &self.business.config;
        Some(TtsRequest {
            text: prepare_for_tts(&seg.text, &c.language, &self.business.pronouncer),
            voice_id: self.business.voice_id.clone()?,
            model: self.cfg.dynamic_model.clone().unwrap_or_else(|| c.voice.dynamic_model.clone()),
            settings: c.voice.settings_for(&seg.delivery),
            language: c.language.clone(),
        })
    }

    /// Neither pre-generated nor already synthesized this process: it will take a while.
    fn needs_live_tts(&self, seg: &SpeechSegment) -> bool {
        self.services.tts.is_some()
            && self.library.get(&seg.delivery, &seg.text).is_none()
            && self.tts_request(seg).is_some_and(|r| self.services.tts_cache.get(&r.cache_key()).is_none())
    }

    /// The business's short opener, from the library only (it must never need TTS itself).
    fn cover_live_tts(&mut self, gain_db: f32) {
        let Some(id) = self.business.config.voice.dynamic_cover.clone() else { return };
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

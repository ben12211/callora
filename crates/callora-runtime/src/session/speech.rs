//! The agent's side of a call: directives executed, speech planned from the voice library
//! or live TTS, playout events, and the hangup or transfer once the last words were heard.

use super::*;

impl Session {
    pub(super) fn execute(&mut self, directives: Vec<Directive>) {
        self.focus_stt();
        self.price_ahead();
        for d in directives {
            match d {
                Directive::Speak { plan, filler } => {
                    if !filler {
                        self.reply_started_at = Some(Instant::now());
                    }
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

    /// Once the ride's pickup and destination cities are known, the price list is asked in the
    /// background: a caller who asks what it costs hears it at once, for whoever is coming.
    fn price_ahead(&mut self) {
        let Some(action) = self.business.config.actions.iter().find_map(|(id, a)| {
            a.backends
                .iter()
                .any(|b| matches!(b, callora_core::config::ActionBackend::PriceBot { .. }))
                .then_some(id.clone())
        }) else {
            return;
        };
        let state = &self.engine.state;
        let place = |slot: &str| {
            let run = state.run.as_ref().into_iter().chain(state.suspended.iter());
            run.filter_map(|r| r.slots.get(slot))
                .find_map(|s| match &s.value {
                    callora_core::values::SlotValue::Place { spoken, address, .. } => {
                        Some(address.clone().unwrap_or_else(|| spoken.clone()))
                    }
                    _ => None,
                })
                .or_else(|| state.place_cities.get(slot).cloned())
        };
        let (Some(from), Some(to)) = (place("pickup"), place("destination")) else { return };
        let route = (callora_core::price_list::city_of(&from), callora_core::price_list::city_of(&to));
        if self.priced.as_ref() == Some(&route) || route.0.is_empty() || route.1.is_empty() {
            return;
        }
        self.priced = Some(route.clone());
        let actions = self.services.actions.clone();
        let business = self.business.clone();
        let input =
            json!({ "run_id": 0, "slots": { "price_from": { "spoken": route.0 }, "price_to": { "spoken": route.1 } } });
        tokio::spawn(async move { actions.warm(&business, &action, input).await });
    }

    /// Queue a plan: each segment from the voice library when it is there, otherwise from
    /// dynamic TTS (streamed, and cached for next time).
    pub(super) fn speak(&mut self, plan: SpeechPlan) {
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

    pub(super) fn tts_request(&self, seg: &SpeechSegment) -> Option<TtsRequest> {
        let c = &self.business.config;
        let spoken =
            prepare_for_tts(&seg.text, &c.language, self.business.pronouncer_for(self.engine.state.address_form));
        Some(TtsRequest {
            text: callora_audio::library::with_tone(&self.business, &seg.response_id, spoken),
            voice_id: self.business.voice_id.clone()?,
            model: self.cfg.dynamic_model.clone().unwrap_or_else(|| c.voice.dynamic_model.clone()),
            settings: c.voice.settings_for(&seg.delivery),
            language: c.language.clone(),
        })
    }

    /// Neither pre-generated nor already synthesized this process: it will take a while.
    pub(super) fn needs_live_tts(&self, seg: &SpeechSegment) -> bool {
        self.services.tts.is_some()
            && self.library.get_loose(&seg.delivery, &seg.text).is_none()
            && self.tts_request(seg).is_some_and(|r| self.services.tts_cache.get(&r.cache_key()).is_none())
    }

    /// The business's short opener, from the library only (it must never need TTS itself).
    pub(super) fn cover_live_tts(&mut self, gain_db: f32) {
        let turn = self.engine.state.turns;
        // Not before the greeting: "אממ, כן. אהלן דוד!" opened calls of known customers.
        if turn == 0 {
            return;
        }
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

    pub(super) async fn on_playout(&mut self, e: PlayoutEvent) -> Option<Ending> {
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

    pub(super) fn agent_busy(&self) -> bool {
        self.queued_items > 0
    }

    pub(super) fn enqueue(&mut self, item: PlayItem) {
        self.queued_items += 1;
        self.playout.enqueue(item);
    }

    pub(super) fn arm_silence(&mut self) {
        self.silence_generation += 1;
        let generation = self.silence_generation;
        let after = Duration::from_millis(self.engine.silence_after_ms());
        let tx = self.events.clone();
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            let _ = tx.send(Ev::Silence { generation });
        });
    }

    pub(super) async fn terminate(&mut self, after: AfterSpeech) -> Ending {
        match after {
            AfterSpeech::Hangup => {
                if let Err(e) = self.services.telephony.hangup(&self.info.call_sid).await {
                    tracing::error!(call = %self.info.call_sid, error = %e, "hangup failed");
                }
                Ending::AgentHungUp
            }
            AfterSpeech::Handoff(summary) => {
                self.services.metrics.handoffs_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if let Some(desk) = self.services.desk.clone() {
                    let settings = self.services.settings.desk(&self.business);
                    let c = &self.business.config;
                    let unavailable = self
                        .business
                        .response(&c.handoff.unavailable_response)
                        .and_then(|r| r.variants.first().cloned())
                        .unwrap_or_default();
                    if desk.transfer(&self.info, &summary, &settings, &c.language, &unavailable).await {
                        return Ending::HandedOff;
                    }
                    let _ = self.services.telephony.hangup(&self.info.call_sid).await;
                    return Ending::AgentHungUp;
                }
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

/// Pieces of a sentence short enough for fast live TTS: split after commas and sentence
/// marks, then merged back so no piece is a lone word ("סגור.") next to a short neighbour.
pub(super) fn split_for_tts(text: &str) -> Vec<String> {
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

pub(super) fn spawn_tts(
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
    #[test]
    fn long_live_sentences_are_split_into_short_pieces() {
        let read_back = "שלושה נוסעים מרבן יוחנן בן זכאי 45, אלעד לאהרונוביץ ראובן 42, בני ברק, עכשיו. לשלוח?";
        let pieces = super::split_for_tts(read_back);
        assert_eq!(pieces.join(" "), read_back, "nothing is lost");
        assert!(pieces.len() >= 3, "{pieces:?}");
        assert!(pieces.iter().all(|p| p.chars().count() <= 40), "{pieces:?}");
        assert_eq!(super::split_for_tts("לאיזה רחוב בבני ברק?"), vec!["לאיזה רחוב בבני ברק?"]);
    }
}

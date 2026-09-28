//! The caller's side of a call: their audio (VAD, barge-in, the audio kept for a second
//! hearing), speech recognition (sessions, reconnects, city-biased sessions) and final
//! transcripts, which go to the agent or, without one, to the rules.

use super::*;

impl Session {
    // -----------------------------------------------------------------------------------
    // Caller audio

    pub(super) fn on_audio(&mut self, frame: Bytes) {
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
    pub(super) fn keep_audio(&mut self, frame: &Bytes, event: Option<&VadEvent>) {
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
    pub(super) fn second_hearing_terms(&self) -> Option<Vec<String>> {
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

    pub(super) fn barge_in(&mut self) {
        self.barge_in_started = Some(Instant::now());
        self.interrupted = true;
        self.playout.cancel();
        self.services.metrics.barge_ins_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::debug!(call = %self.info.call_sid, rms = self.vad.last_rms, "barge-in: caller talked over the agent");
    }

    // -----------------------------------------------------------------------------------
    // Transcripts and understanding

    pub(super) fn on_stt(&mut self, event: SttEvent) {
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

    pub(super) fn on_final(&mut self, text: String) {
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
    pub(super) fn on_noise(&mut self, text: &str) {
        tracing::info!(call = %self.info.call_sid, caller = %text, "ignored as noise");
        if self.agent_busy() {
            return;
        }
        if std::mem::take(&mut self.interrupted) {
            let directives = self.engine.replay_last();
            self.execute(directives);
        } else {
            // The silence reprompt as a backstop; the question again first, unless the caller
            // goes on talking.
            self.arm_silence();
            let generation = self.silence_generation;
            let tx = self.events.clone();
            tokio::spawn(async move {
                tokio::time::sleep(UNHEARD_WAIT).await;
                let _ = tx.send(Ev::Unheard { generation });
            });
        }
    }

    // -----------------------------------------------------------------------------------
    // Recognition hints

    /// Recognition hints: the city's streets when the call waits for one, then the business's
    /// own words (the recognizer keeps as many from the front as it takes).
    pub(super) fn stt_keyterms(&self, city: Option<&str>) -> Vec<String> {
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

    /// "בני ברק" given, its street asked next: open a session biased with its streets beside
    /// the live one; once the question is about something else, one with the business's
    /// words only. Recognition cannot be re-biased mid-session, and "אהרונוביץ" in an
    /// Ashkenazi accent came back as "עונה מ-32" without the hint, while a city's streets
    /// left on turned the caller's name into one of them.
    pub(super) fn focus_stt(&mut self) {
        let city = self.engine.street_focus();
        if self.services.gazetteer.is_none()
            || self.stt_city == city
            || self.stt_opening.as_ref() == Some(&city)
            || self.stt_next.as_ref().is_some_and(|(c, _)| *c == city)
        {
            return;
        }
        self.stt_opening = Some(city.clone());
        let stt = self.services.stt.clone();
        let language = self.business.config.language.clone();
        let keyterms = self.stt_keyterms(city.as_deref());
        let tx = self.events.clone();
        tokio::spawn(async move {
            let result = stt.open(&language, &keyterms).await;
            let _ = tx.send(Ev::SttFocused { city, result });
        });
    }

    /// The biased session takes over between utterances only: never while the caller is
    /// talking or a transcript is still due from the live session.
    pub(super) fn swap_stt_if_ready(&mut self) {
        if self.stt_next.is_none() || self.vad.is_speaking() || self.finalize_sent_at.is_some() {
            return;
        }
        let Some((city, session)) = self.stt_next.take() else { return };
        if let Some(old) = self.stt.take() {
            let _ = old.input.try_send(SttInput::Close);
        }
        match &city {
            Some(city) => {
                tracing::info!(call = %self.info.call_sid, %city, "recognition biased with the city's streets")
            }
            None => tracing::info!(call = %self.info.call_sid, "recognition back to the business's words"),
        }
        self.stt = Some(session);
        self.stt_city = city;
    }
}

/// A sentence the caller broke off: the recognizer's trailing "..." or "-", or a last word
/// that is a lone Hebrew letter, a prefix waiting for its word ("מ", "אני צריך ל"), which
/// recognizers that mark nothing (Deepgram) return when the caller hesitates: a live call
/// answered "מ" with a question while the caller was saying "...בן זכאי 45".
pub(super) fn is_unfinished(text: &str) -> bool {
    let t = text.trim_end();
    let lone_letter = t
        .trim_end_matches(['.', ',', '?', '!'])
        .split_whitespace()
        .last()
        .is_some_and(|w| w.chars().count() == 1 && w.chars().all(|c| ('א'..='ת').contains(&c)));
    t.ends_with("...") || t.ends_with('…') || t.ends_with('-') || lone_letter
}

#[cfg(test)]
mod tests {
    use super::is_unfinished;

    #[test]
    fn broken_off_sentences_are_recognised() {
        for t in ["ואני רוצה להגיע ל...", "יעני, מתל-ב-ב-ב-ב-", "אני נוסע ל… ", "מ", "אני צריך ל", "מ."]
        {
            assert!(is_unfinished(t), "{t}");
        }
        for t in ["לתל אביב.", "מה המצב?", "3-4 נוסעים", "לא", "כן", "12"] {
            assert!(!is_unfinished(t), "{t}");
        }
    }
}

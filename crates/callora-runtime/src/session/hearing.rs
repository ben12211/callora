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
                // The words of the speech before are no guess at this one.
                self.last_partial.clear();
                self.cancel_no_words();
                self.silence_generation += 1;
                self.speech_started_at = Some(Instant::now());
                self.speech_gap = self.last_speech_end.map(|t| t.elapsed());
                self.speech_count += 1;
                self.utterance_heard = false;
                self.voiced_ms = 0;
                if self.pending_agent.as_ref().is_some_and(|p| p.speculative) {
                    // The caller went on talking: the guess was about half a sentence.
                    if let Some(p) = self.pending_agent.take() {
                        p.task.abort();
                    }
                }
                // Not yet: noise must not cut the agent off. Words, or a voice that goes on,
                // confirm it (below and in `on_stt`).
                if self.agent_busy() {
                    self.barge_pending = true;
                }
            }
            Some(VadEvent::SpeechEnded) => {
                let now = Instant::now();
                self.speech_ended_at = Some(now);
                self.last_speech_end = Some(now);
                self.barge_pending = false;
                if !self.utterance_heard {
                    self.arm_no_words();
                }
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
                        let tx = self.events.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(FINAL_OVERDUE).await;
                            let _ = tx.send(Ev::FinalOverdue { sent_at: now });
                        });
                    }
                }
                self.speculate();
            }
            None => {}
        }
        if self.vad.is_speaking() {
            self.voiced_ms += frame.len() as u64 / 8;
        }
        if self.barge_pending {
            self.confirm_barge_in(None);
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
            None if self.vad.is_speaking() && self.utterance.len() < MAX_BYTES => {
                self.utterance.extend_from_slice(frame);
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

    /// The caller's voice over the agent is a barge-in once it has words in it (and, during a
    /// read-back, more than "כן", "אהה"), or once it has gone on long enough.
    pub(super) fn confirm_barge_in(&mut self, partial: Option<&str>) {
        if !self.agent_busy() {
            self.barge_pending = false;
            return;
        }
        let reading_back = self.engine.context().awaiting_confirmation;
        let words = partial.is_some_and(|text| {
            let (u, _) = fast_path(&self.business, &self.engine.context(), text);
            !u.noise && !self.engine.is_backchannel(text) && !self.engine.is_hello(text)
        });
        let enough = if reading_back {
            BARGE_CONFIRM_READ_BACK
        } else if self.engine.state.turns == 0 {
            BARGE_CONFIRM_GREETING
        } else {
            BARGE_CONFIRM
        };
        if words || Duration::from_millis(self.voiced_ms) >= enough {
            self.barge_pending = false;
            self.barge_in();
        }
    }

    pub(super) fn barge_in(&mut self) {
        self.barge_in_started = Some(Instant::now());
        self.interrupted = true;
        self.cut_read_back = self.engine.context().awaiting_confirmation;
        self.playout.cancel();
        self.services.metrics.barge_ins_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Info: "it stops mid-sentence" is either this or the audio; the log tells which.
        tracing::info!(
            call = %self.info.call_sid,
            voiced_ms = self.voiced_ms,
            partial = %self.last_partial,
            rms = self.vad.last_rms,
            "barge-in: caller talked over the agent"
        );
    }

    // -----------------------------------------------------------------------------------
    // Transcripts and understanding

    pub(super) fn on_stt(&mut self, event: SttEvent) {
        match event {
            SttEvent::Partial(text) => {
                if !text.trim().is_empty() && self.no_words_task.is_some() {
                    // Progress from ASR buys another full grace period for the final words.
                    self.arm_no_words();
                }
                if self.barge_pending && !text.trim().is_empty() {
                    self.confirm_barge_in(Some(&text));
                }
                self.last_partial = text;
            }
            SttEvent::Final(text) => {
                self.stt_reconnects = 0;
                if !text.trim().is_empty() {
                    self.utterance_heard = true;
                    self.cancel_no_words();
                }
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
        // "תודה" to "משהו נוסף?": the goodbye, without asking anyone.
        if !self.agent_busy() && self.pending_agent.is_none() && self.pending_llm.is_none() {
            if let Some(d) = self.engine.on_closing(&text) {
                tracing::info!(call = %self.info.call_sid, caller = %text, "caller");
                self.interrupted = false;
                self.silence_generation += 1;
                return self.execute(d);
            }
            if let Some(d) = self.engine.on_no_to_optional(&text) {
                tracing::info!(call = %self.info.call_sid, caller = %text, "caller");
                self.interrupted = false;
                self.silence_generation += 1;
                return self.execute(d);
            }
        }
        if self.pending_llm.is_none() {
            let (u, _) = fast_path(&self.business, &self.engine.context(), &text);
            // "טוב", "תודה" are noise when nothing waits for them, and an answer when the
            // call waits for a short one.
            if u.noise && !self.engine.takes_short_answer(&text) {
                return self.on_noise(&text);
            }
        }
        // "כן", "בסדר" while the details are read back: the caller listening. Neither a stop nor
        // a yes; after a read-back cut off, the read-back again.
        if self.engine.is_backchannel(&text) {
            let yes = self.business.affirm.find(&callora_core::text::normalize(&text)).is_some();
            // The same read-back again (a silence's reprompt), heard to its end before: a yes
            // over it is the answer now. A live caller said "כן. כן." and "כן" over it, both
            // dropped, then "לא" in frustration, taken for a correction.
            let heard_before =
                self.read_back_heard.is_some() && self.read_back_heard == self.engine.last_question_text();
            if self.agent_busy() && yes && heard_before {
                tracing::info!(call = %self.info.call_sid, caller = %text, "a yes over a read-back heard before; the answer");
                self.barge_in();
                self.cut_read_back = false;
            } else if self.agent_busy() {
                tracing::info!(call = %self.info.call_sid, caller = %text, "said over the read-back; not an answer");
                if yes {
                    self.yes_over_read_back = Some((Instant::now(), text));
                }
                return;
            }
            if std::mem::take(&mut self.cut_read_back) {
                tracing::info!(call = %self.info.call_sid, caller = %text, "a yes to a read-back cut off; reading it again");
                self.interrupted = false;
                self.silence_generation += 1;
                let d = self.engine.replay_last();
                return self.execute(d);
            }
        }
        self.cut_read_back = false;
        self.yes_over_read_back = None;
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
        // "הלו?": "כן, אני פה." and the question again.
        if self.engine.is_hello(text) && self.pending_agent.is_none() {
            self.interrupted = false;
            let d = self.engine.on_hello();
            if d.is_empty() {
                self.arm_silence();
            }
            return self.execute(d);
        }
        if std::mem::take(&mut self.interrupted) {
            self.noise_heard(true);
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

    /// Noise the caller could not have meant: "the line is noisy" and what was cut off (or the
    /// question) again; when nothing is to be said, the silence reprompt waits.
    pub(super) fn noise_heard(&mut self, interrupted: bool) {
        let d = if interrupted { self.engine.on_noise(true) } else { self.engine.on_unheard() };
        if d.is_empty() {
            self.arm_silence();
        }
        self.execute(d);
    }

    pub(super) fn cancel_no_words(&mut self) {
        self.no_words_generation += 1;
        if let Some(task) = self.no_words_task.take() {
            task.abort();
        }
    }

    pub(super) fn arm_no_words(&mut self) {
        self.cancel_no_words();
        let utterance = self.speech_count;
        let generation = self.no_words_generation;
        let tx = self.events.clone();
        self.no_words_task = Some(tokio::spawn(async move {
            tokio::time::sleep(NO_WORDS_WAIT).await;
            let _ = tx.send(Ev::NoWords { utterance, generation });
        }));
    }

    // -----------------------------------------------------------------------------------
    // Recognition hints

    /// Recognition hints: the city's streets when the call waits for one, the towns of the
    /// business's area always, then the business's own words (the recognizer keeps as many
    /// from the front as it takes). The area from the first word: on 150 recorded utterances
    /// it turned "מלעד לבנון" into "מאלעד לבני ברק", "מאילת" into "מאלעד", "מפרט" into "מאפרת".
    pub(super) fn stt_keyterms(&self, city: Option<&str>) -> Vec<String> {
        let mut terms = Vec::new();
        let area = &self.business.config.service_area;
        if let (Some(city), Some(g)) = (city, &self.services.gazetteer) {
            terms.push(city.to_string());
            terms.extend(area.iter().cloned());
            terms.extend(g.street_keyterms(city, 38));
        } else {
            terms.extend(area.iter().cloned());
        }
        if self.services.stt.wants_business_words() {
            terms.extend(self.business.stt_keyterms());
        }
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

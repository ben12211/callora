//! The conversation agent's turn: the request, its reply streamed in (a recorded phrase the
//! moment its id arrives, sentences of `say` as they complete, values checked before any
//! word plays), and the decision handed to the engine, which enforces the rules.

use super::*;

impl Session {
    // -----------------------------------------------------------------------------------
    // The conversation agent

    pub(super) fn agent_mode(&self) -> bool {
        self.services.agent.is_some() && self.business.config.agent.is_some()
    }

    /// At the end of speech, start the agent on the recognizer's partial text instead of
    /// waiting ~230 ms for the final transcript. Its speech is held until the final
    /// transcript confirms the words; a different final starts over.
    pub(super) fn speculate(&mut self) {
        if !self.cfg.agent_speculate || !self.agent_mode() || self.pending_agent.is_some() || self.pending_llm.is_some()
        {
            return;
        }
        let text = self.last_partial.trim().to_string();
        if text.is_empty() {
            return;
        }
        let (u, needs_llm) = fast_path(&self.business, &self.engine.context(), &text);
        if u.noise || self.engine.fast_lane(&u, needs_llm) {
            return;
        }
        self.start_agent(text, true);
    }

    pub(super) fn on_final_agent(&mut self, text: String) {
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
        if self.engine.fast_lane(&fast, needs_llm) {
            return self.understood(fast);
        }
        if self.info.from.as_ref().is_some_and(|f| self.cfg.sample_audio_from.contains(f))
            && !self.last_utterance.is_empty()
        {
            self.services.store.record(CallRecord::Utterance {
                call_id: self.info.call_id,
                heard: transcript.clone(),
                audio: self.last_utterance.clone(),
            });
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

    pub(super) fn adopt_speculation(&mut self) {
        let Some(p) = self.pending_agent.as_mut() else { return };
        p.speculative = false;
        let held = std::mem::take(&mut p.held);
        let phrase = p.held_phrase.take();
        let done = p.done.take();
        if let Some(id) = phrase {
            self.agent_phrase(id);
        }
        for sentence in held {
            self.agent_sentence(sentence);
        }
        if let Some((result, rest, usage)) = done {
            self.finish_agent(result, rest, usage);
        }
    }

    pub(super) fn start_agent(&mut self, transcript: String, speculative: bool) {
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
                let (mut stream, usage) = model.stream_metered(&request).await?;
                let mut say = SayStream::default();
                let mut reply = String::new();
                let mut fields_sent = false;
                let mut phrase_sent = false;
                while let Some(delta) = stream.next().await {
                    let delta = delta?;
                    reply.push_str(&delta);
                    let sentences = say.push(&delta);
                    if !fields_sent {
                        if let Some(fields) = say.fields() {
                            fields_sent = true;
                            let _ = says.send(Ev::AgentFields { turn, fields });
                        }
                    }
                    // A read-back or a submit: the engine speaks the words with what follows.
                    if matches!(say.action(), Some(AgentAction::ReadBack | AgentAction::Submit | AgentAction::EndCall))
                    {
                        continue;
                    }
                    // A recorded phrase plays the moment its id is complete (after the fields,
                    // which were checked first).
                    if fields_sent && !phrase_sent {
                        if let Some(id) = say.phrase() {
                            phrase_sent = true;
                            let _ = says.send(Ev::AgentPhrase { turn, id });
                        }
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
                // The token counts come with the stream's last event.
                let usage = tokio::time::timeout(Duration::from_millis(200), usage).await.ok().and_then(Result::ok);
                anyhow::Ok((value, rest, usage))
            };
            let (result, rest, usage) = match tokio::time::timeout(timeout, decide).await {
                Ok(Ok((value, rest, usage))) => (Ok(value), rest, usage),
                Ok(Err(e)) => (Err(e), None, None),
                Err(_) => (Err(anyhow::anyhow!("timed out after {timeout:?}")), None, None),
            };
            let _ = tx.send(Ev::AgentDone { turn, result, rest, usage });
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
            held_phrase: None,
            phrase: None,
            fields: Vec::new(),
            partial_phrase: None,
            done: None,
            hold_say: false,
        });
    }

    /// One sentence of the agent's reply, as it streams in. A sentence that begins one of
    /// the recorded phrases waits for the next one, so the phrase plays as its clip instead
    /// of half of it going through live TTS.
    pub(super) fn agent_sentence(&mut self, sentence: String) {
        let Some(p) = self.pending_agent.as_ref() else { return };
        if p.hold_say {
            return;
        }
        // The phrase said it already: its words again, or a second question.
        if p.phrase.as_deref().is_some_and(|id| !agent::say_after_phrase(&self.business, id, &sentence)) {
            tracing::info!(call = %self.info.call_sid, said = %sentence, "dropped: the recorded phrase already asked");
            return;
        }
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

    pub(super) fn agent_sentence_text(&mut self, text: String) {
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

    pub(super) fn continues_a_phrase(&self, text: &str) -> bool {
        let w = words(text);
        self.phrase_words.iter().any(|ph| ph.len() > w.len() && ph.starts_with(&w))
    }

    pub(super) fn library_has(&self, text: &str) -> bool {
        let delivery = self.engine.state.delivery.clone().unwrap_or_else(|| "normal".into());
        self.library.get_loose(&delivery, text).is_some()
    }

    /// The agent's recorded phrase, the moment its id arrived: no text to wait for, no TTS.
    pub(super) fn agent_phrase(&mut self, id: String) {
        let Some(p) = self.pending_agent.as_mut() else { return };
        if p.hold_say || p.phrase.is_some() {
            return;
        }
        if p.speculative {
            p.held_phrase = Some(id);
            return;
        }
        // It asks for a value this same reply passes: the engine decides once the value is
        // checked (the next question if it was taken, this one if it was not).
        let answered = self.engine.slot_asked_by(&id).is_some_and(|slot| p.fields.iter().any(|(s, _)| *s == slot));
        if answered {
            p.hold_say = true;
            return;
        }
        let Some(plan) = self.engine.render_response(&id) else { return };
        if let Some(p) = self.pending_agent.as_mut() {
            p.phrase = Some(id.clone());
            p.spoken.push(plan.text());
        }
        self.play_agent(plan, &id);
    }

    /// Play one sentence of the agent's reply now.
    pub(super) fn say_now(&mut self, sentence: &str) {
        let delivery = self.engine.state.delivery.clone().unwrap_or_else(|| "normal".into());
        self.play_agent(SpeechPlan::free(sentence, &delivery, self.engine.state.gain_db), "agent");
    }

    fn play_agent(&mut self, plan: SpeechPlan, response: &str) {
        let text = plan.text();
        let recorded = plan.segments.iter().all(|s| self.library.get_loose(&s.delivery, &s.text).is_some());
        if let Some(c) = &mut self.clock {
            c.agent_first.get_or_insert_with(Instant::now);
            if c.audio.is_empty() {
                c.audio = if recorded { "recorded" } else { "live tts" };
            }
        }
        self.services.store.record(CallRecord::Turn {
            call_id: self.info.call_id,
            speaker: "agent".into(),
            text: text.clone(),
            detail: json!({ "responses": [response] }),
        });
        tracing::info!(call = %self.info.call_sid, agent = %text, "agent");
        self.speak(plan);
    }

    pub(super) fn finish_agent(&mut self, result: anyhow::Result<Value>, rest: Option<String>, usage: Option<Usage>) {
        let Some(mut p) = self.pending_agent.take() else { return };
        let elapsed = p.started.elapsed().as_millis() as u64;
        self.services.metrics.llm_latency.observe(elapsed);
        if let Some(u) = &usage {
            self.usage.add(u);
        }
        // The caller's words and what the agent made of them: the calls page shows them, and a
        // call that went wrong becomes an eval case from them.
        self.services.store.record(CallRecord::Turn {
            call_id: self.info.call_id,
            speaker: "caller".into(),
            text: p.transcript.clone(),
            detail: json!({
                "route": "agent",
                "reply": result.as_ref().ok(),
                "error": result.as_ref().err().map(|e| format!("{e:#}")),
                "second_hearing": self.engine.state.second_hearing,
                "decision_ms": elapsed,
                "usage": usage,
            }),
        });
        match result {
            Ok(reply) => {
                // Whatever is left, with the start of a phrase that was waiting for it.
                let tail = [p.partial_phrase.take(), rest].into_iter().flatten().collect::<Vec<_>>().join(" ");
                let after_phrase =
                    p.phrase.as_deref().is_none_or(|id| agent::say_after_phrase(&self.business, id, &tail));
                if !tail.is_empty() && !p.hold_say && after_phrase {
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

    pub(super) fn understood(&mut self, u: Understanding) {
        self.services.store.record(CallRecord::Turn {
            call_id: self.info.call_id,
            speaker: "caller".into(),
            text: u.transcript.clone(),
            detail: serde_json::to_value(&u).unwrap_or(Value::Null),
        });
        let directives = self.engine.on_utterance(u);
        self.execute(directives);
    }
}

/// The same words, ignoring punctuation, spacing and case: a partial transcript that
/// already says what the final one says.
pub(super) fn same_words(a: &str, b: &str) -> bool {
    words(a) == words(b)
}

/// The words of a sentence, without punctuation or case.
pub(super) fn words(s: &str) -> Vec<String> {
    s.split_whitespace()
        .map(|w| w.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase())
        .filter(|w| !w.is_empty())
        .collect()
}

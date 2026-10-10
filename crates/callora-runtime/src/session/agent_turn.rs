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
            tracing::info!(call = %self.info.call_sid, "no partial transcript at the end of speech; not speculating");
            return;
        }
        let (u, needs_llm) = fast_path(&self.business, &self.engine.context(), &text);
        if u.noise || self.engine.fast_lane(&u, needs_llm) {
            return;
        }
        // A street or city the lists do not know yet is heard a second time first, which waits
        // for the final words anyway; a hit here would skip it.
        if self.second_hearing_expected(&text).is_some() {
            tracing::info!(call = %self.info.call_sid, "a second hearing is due; not speculating");
            return;
        }
        self.start_agent(text, true);
    }

    /// The owner's test calls keep each utterance's audio, to compare recognizers.
    fn record_utterance(&self, transcript: &str) {
        if self.info.from.as_ref().is_some_and(|f| self.cfg.sample_audio_from.contains(f))
            && !self.last_utterance.is_empty()
        {
            self.services.store.record(CallRecord::Utterance {
                call_id: self.info.call_id,
                heard: transcript.to_string(),
                audio: self.last_utterance.clone(),
            });
        }
    }

    pub(super) fn on_final_agent(&mut self, text: String) {
        self.last_partial.clear();
        self.overlap = self.overlaps_last_reply();
        let mut transcript = text;
        if let Some(p) = self.pending_agent.take() {
            if p.speculative && same_words(&p.transcript, &transcript) {
                tracing::info!(call = %self.info.call_sid, caller = %transcript, "caller");
                self.services.metrics.turns_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if let Some(c) = &mut self.clock {
                    c.speculative_hit = true;
                }
                self.record_utterance(&transcript);
                self.pending_agent = Some(p);
                return self.adopt_speculation();
            }
            if p.speculative {
                tracing::info!(call = %self.info.call_sid, partial = %p.transcript, r#final = %transcript, "speculation missed");
            }
            p.task.abort();
            // A new sentence while the agent was still thinking about the previous one:
            // they are one utterance. Also once its reply began ("סגור, לאן?" over "ושלושה
            // אנשים"): that reply's values were never taken, and the caller would be asked
            // for them again. What it said is remembered, so the next reply knows.
            if !p.speculative {
                if !p.spoken.is_empty() {
                    self.engine.state.remember(Speaker::Agent, &p.spoken.join(" "));
                }
                transcript = format!("{} {transcript}", p.transcript);
            }
        }
        self.services.metrics.turns_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tracing::info!(call = %self.info.call_sid, caller = %transcript, "caller");
        let (fast, needs_llm) = fast_path(&self.business, &self.engine.context(), &transcript);
        if self.engine.fast_lane(&fast, needs_llm) {
            return self.understood(fast);
        }
        self.record_utterance(&transcript);
        // A city or street expected: hear the words once more, with every name expected as a
        // hint, before the agent decides ("זה ביתר" came back "זה יותר", "אהרונוביץ" as
        // "עונה מ-32"). The agent gets both.
        if let Some((_, earlier)) = self.second_pending.take() {
            transcript = format!("{earlier} {transcript}");
        }
        if let (Some((question, names)), Some(t)) =
            (self.second_hearing_expected(&transcript), self.services.second_hearing.clone())
        {
            self.second_ids += 1;
            let id = self.second_ids;
            let audio = self.last_utterance.clone();
            let language = self.business.config.language.clone();
            let tx = self.events.clone();
            let started = Instant::now();
            let call = self.info.call_sid.clone();
            tokio::spawn(async move {
                let heard =
                    tokio::time::timeout(SECOND_HEARING_WAIT, t.hear(&audio, &language, &question, &names)).await;
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
            // A second hearing adds about a second before the agent even starts: the wait is
            // known, so the thinking sound plays now rather than after the silence.
            self.thinking_sound();
            return;
        }
        self.engine.state.second_hearing = None;
        self.start_agent(transcript, false);
    }

    /// The caller began these words before the agent's last reply started (and not long
    /// ago): they finish the previous answer rather than answer the new question.
    pub(super) fn overlaps_last_reply(&self) -> bool {
        match (self.speech_started_at, self.reply_started_at) {
            (Some(began), Some(reply)) => {
                began < reply
                    && reply.elapsed() < Duration::from_secs(6)
                    && self.speech_gap.is_none_or(|gap| gap < CONTINUATION_GAP)
            }
            _ => false,
        }
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

    /// "שנייה... רגע...": the agent thinking, when the caller would otherwise wait in silence.
    /// Once a turn at most, and not two turns in a row.
    pub(super) fn thinking_sound(&mut self) {
        let caller_turn = self.engine.state.turns;
        let recent = self.filler_turn.is_some_and(|t| t == caller_turn || t + 1 == caller_turn);
        if recent || self.agent_busy() {
            return;
        }
        let Some(id) = self.business.config.agent.as_ref().and_then(|a| a.thinking_filler.clone()) else { return };
        if let Some(plan) = self.engine.render_response(&id) {
            tracing::info!(call = %self.info.call_sid, "a known wait: the agent is heard thinking");
            self.filler_turn = Some(caller_turn);
            self.speak(plan);
        }
    }

    pub(super) fn start_agent(&mut self, transcript: String, speculative: bool) {
        let (Some(model), Some(cfg)) = (self.services.agent.clone(), self.business.config.agent.clone()) else {
            return;
        };
        self.turn += 1;
        let turn = self.turn;
        self.engine.state.continues_answer = !speculative && std::mem::take(&mut self.overlap);
        if self.engine.state.continues_answer {
            tracing::info!(call = %self.info.call_sid, caller = %transcript, "the caller began before the last reply: finishing the previous answer");
        }
        self.engine.hint_towns(&transcript);
        let request = agent::build_request(&self.business, &self.engine.state, &transcript);
        self.engine.state.continues_answer = false;
        let timeout = Duration::from_millis(cfg.timeout_ms);
        let tx = self.events.clone();
        let business = self.business.clone();
        let task = tokio::spawn(async move {
            let says = tx.clone();
            let decide = async move {
                let (mut stream, usage) = model.stream_metered(&request).await?;
                let mut say = SayStream::default();
                let mut reply = String::new();
                let mut fields_sent = false;
                let mut asks_sent = false;
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
                    // What the question asks for, before its words: a question that moves on
                    // past an unanswered one is held.
                    if fields_sent && !asks_sent {
                        if let Some(asks) = say.asks() {
                            asks_sent = true;
                            let _ = says.send(Ev::AgentAsks { turn, asks });
                        }
                    }
                    // A read-back or a submit: the engine speaks the words with what follows.
                    if matches!(say.action(), Some(AgentAction::ReadBack | AgentAction::Submit | AgentAction::EndCall))
                    {
                        continue;
                    }
                    // A recorded phrase plays the moment its id is complete (after the fields
                    // and what the question asks for, which were checked first).
                    if asks_sent && !phrase_sent {
                        if let Some(id) = say.phrase() {
                            phrase_sent = true;
                            let _ = says.send(Ev::AgentPhrase { turn, id });
                        }
                    }
                    for sentence in sentences {
                        // "רגע, מעביר למוקדן" with no transfer (see `announces_transfer`).
                        if say.action() != Some(AgentAction::Transfer) && business.announces_transfer(&sentence) {
                            tracing::info!(%sentence, "a transfer announced without one; not said");
                            continue;
                        }
                        let _ = says.send(Ev::AgentSay { turn, sentence });
                    }
                }
                let value: Value = serde_json::from_str(&reply)
                    .map_err(|e| anyhow::anyhow!("the agent's reply is not JSON ({e}): {reply}"))?;
                let held =
                    matches!(say.action(), Some(AgentAction::ReadBack | AgentAction::Submit | AgentAction::EndCall));
                let rest = say
                    .rest()
                    .filter(|_| !held)
                    .filter(|r| say.action() == Some(AgentAction::Transfer) || !business.announces_transfer(r));
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
        // A curse: the engine answers it, never the agent's own words ("סבבה." to a lewd note).
        let decided = self.engine.decisive_intent(&transcript).is_some() || self.business.is_abusive(&transcript);
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
            // Words that decide the task (a price question): the agent's own words may be for
            // another (a booking's next question), so the engine speaks.
            hold_say: decided,
            asks: Vec::new(),
            held_for: None,
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
        // A question in its own words with no `asks`: what it asks is read from its words, and
        // one that goes past a detail still missing is held like any other.
        if p.asks.is_empty() && p.spoken.is_empty() {
            let asks = self.engine.asks_in(&sentence);
            if !asks.is_empty() {
                let held = self.engine.moves_on(&p.fields, &asks);
                if let Some(slot) = held {
                    tracing::info!(call = %self.info.call_sid, %slot, ?asks, said = %sentence, "a question in the agent's words moves on past an open one; held");
                    if let Some(p) = self.pending_agent.as_mut() {
                        p.hold_say = true;
                        p.held_for = Some(slot);
                    }
                    return;
                }
            }
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
        // The very question it asked last, again: "סליחה, לא שמעתי טוב." first, so it does not
        // sound like a machine stuck on one line (15 times in 74 recorded calls).
        // Only when the answer gave nothing: a question after details were taken moves on, even
        // when it is the greeting's own ("לאן נוסעים?" after the pickup).
        // And only a question asked by name both times (the greeting's asks nothing by name).
        let asked_again =
            !self.engine.state.last_asks.is_empty() && p.asks.iter().any(|a| self.engine.state.last_asks.contains(a));
        let first = p.spoken.is_empty() && p.fields.is_empty() && asked_again;
        if first && self.repeats_last_question(&text) {
            if let Some(sorry) = self.engine.render_response("did_not_catch").map(|plan| plan.text()) {
                tracing::info!(call = %self.info.call_sid, question = %text, "the same question again: said it did not catch the answer");
                if let Some(p) = self.pending_agent.as_mut() {
                    p.spoken.push(sorry.clone());
                }
                self.say_now(&sorry);
            }
        }
        if let Some(p) = self.pending_agent.as_mut() {
            p.spoken.push(text.clone());
        }
        self.say_now(&text);
    }

    /// The agent's last words before the caller's were this very question.
    pub(super) fn repeats_last_question(&self, text: &str) -> bool {
        let asks = text.trim_end().ends_with('?');
        let text = callora_core::text::normalize(text);
        asks && !text.is_empty()
            && self.engine.state.history.iter().rev().find(|t| t.speaker == Speaker::Agent).is_some_and(|t| {
                t.text.trim_end().ends_with('?') && callora_core::text::normalize(&t.text).ends_with(&text)
            })
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
        // A refusal ("רק במוניות") waits for the whole decision: to "כמה זה יוצא לי?" the engine
        // takes the business's task instead, and the agent's own task may show it is one (the
        // caller's words garbled, the decision a price question). The engine says it otherwise.
        if self.business.response(&id).is_some_and(|r| r.alone) {
            tracing::info!(call = %self.info.call_sid, phrase = %id, "a refusal; held for the decision");
            if let Some(p) = self.pending_agent.as_mut() {
                p.hold_say = true;
            }
            return;
        }
        // It asks for a value this same reply passes: the engine decides once the value is
        // checked (the next question if it was taken, this one if it was not).
        let asked = self.engine.slot_asked_by(&id);
        let answered = asked.as_ref().is_some_and(|slot| p.fields.iter().any(|(s, _)| s == slot));
        if answered {
            p.hold_say = true;
            return;
        }
        // Out of the task's order: the engine asks the next question instead.
        if let Some(slot) = asked.clone() {
            if let Some(next) = self.engine.out_of_order(&p.transcript, &p.fields, &[slot]) {
                tracing::info!(call = %self.info.call_sid, phrase = %id, %next, "the phrase is out of the task's order; held");
                if let Some(p) = self.pending_agent.as_mut() {
                    p.hold_say = true;
                    p.held_for = Some(next);
                }
                return;
            }
        }
        // One that asks past a place given only in part: the engine asks for the place first.
        if self.engine.phrase_skips_a_place(&p.transcript, &p.fields, &id) {
            tracing::info!(call = %self.info.call_sid, phrase = %id, "the phrase skips a place given in part; held");
            if let Some(p) = self.pending_agent.as_mut() {
                p.hold_say = true;
            }
            return;
        }
        // A phrase that asks for another detail while one asked earlier is still missing (a
        // reply whose `asks` did not say so).
        if p.asks.is_empty() {
            if let Some(slot) = asked.and_then(|a| self.engine.moves_on(&p.fields, &[a])) {
                tracing::info!(call = %self.info.call_sid, %slot, phrase = %id, "the phrase moves on past an open question; held");
                if let Some(p) = self.pending_agent.as_mut() {
                    p.hold_say = true;
                    p.held_for = Some(slot);
                }
                return;
            }
        }
        let Some(plan) = self.engine.render_phrase(&id) else { return };
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
        self.reply_started_at = Some(Instant::now());
        let text = plan.text();
        let recorded = plan.segments.iter().all(|s| self.library.get_loose(&s.delivery, &s.text).is_some());
        if let Some(c) = &mut self.clock {
            if c.agent_first.is_none() {
                c.agent_first = Some(Instant::now());
                self.services.metrics.agent_first.observe(c.speech_end.elapsed().as_millis() as u64);
            }
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
                "held_for_open_question": p.held_for,
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
                // How the agent read the caller: the next turn is told, so the mood of the call
                // is remembered and not only that of the last sentence.
                let tone = agent::tone(&reply);
                self.engine.state.remember_mood(tone);
                tracing::info!(call = %self.info.call_sid, action = ?decision.action, task = ?decision.task, fields = ?decision.fields, tone = tone.as_str(), "agent decision");
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

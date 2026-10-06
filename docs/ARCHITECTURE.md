# Callora V2 architecture

This document explains how the [product architecture](callora_voice_agent_architecture.md)
is implemented. Section numbers (§) refer to that document.

## 1. Principles

1. **An AI agent runs the call; the engine keeps it safe.** Every turn, an LLM sees the
   whole conversation and the task so far and decides what to say and do. It never acts
   on its own: its decision is a small JSON object that the engine applies under the
   business's hard rules (values through typed parsers, nothing sent without a read-back
   and a real yes, no hangup without a goodbye).
2. **The engine is generic; businesses are data.** `callora-core` knows about intents,
   pipelines, slots, rules, actions and responses, and nothing about taxis. Taxi behaviour,
   including the agent's persona, rules and examples, lives in
   [`businesses/taxi.json`](../businesses/taxi.json) (§21–22). A second business (a clinic,
   `crates/callora-core/tests/fixtures/clinic.json`) runs through the same code as a test.
3. **State is explicit.** `CallState` holds the task in progress, each slot's value with
   its confidence and provenance, what was just asked, whether a read-back is pending,
   suspended tasks and the conversation. The model never owns state.
4. **Decisions are pure; I/O happens elsewhere.** The engine turns decisions into
   `Directive`s (speak, run action, hand off, hang up) that the runtime executes, so every
   rule is a unit test (`crates/callora-core/tests/taxi.rs`).
5. **Recordings first.** Most of what a dispatcher says is a fixed question. Those are
   pre-recorded, and the agent names them by id, so they play the moment the id arrives.
6. **Measured, not guessed.** The model's behaviour is measured on real conversations
   (`callora eval`), and every call's decisions, latency and cost are on the calls page.

## 2. A call, end to end

```text
Twilio ──POST /voice (signed)──▶ route by To → business
        │                        start customer lookup (in parallel)
        │◀── <Connect><Stream> + call-bound HMAC token
        │
        ├──WS /media start──▶ Session actor (one tokio task per call)
        │                       ├─ greeting from the voice library: plays on the next frame
        │                       ├─ agent's prompt cache warmed; STT connects in the background
 caller audio ─────────────────▶├─ VAD: speech start → barge-in (cancel + Twilio clear)
        │                       │       speech end   → STT finalize (fast endpoint)
        │                       ├─ final transcript
        │                       │     ├─ filler words only → ignored as line noise
        │                       │     ├─ one certain meaning ("רגע", "מה?", a plain yes to the
        │                       │     │   read-back) → the engine alone (the fast lane)
        │                       │     └─ anything else → the agent (streamed)
        │                       │           ├─ fields  → checked before a word plays
        │                       │           ├─ phrase  → its recording plays at once
        │                       │           ├─ say     → sentence by sentence, library or TTS
        │                       │           └─ action  → the engine applies the decision
        │                       ├─ Engine → directives
        │                       │     ├─ Speak  → library clip | TTS stream
        │                       │     ├─ Action → business backend (async) → result → Engine
        │                       │     └─ Hangup / Handoff → after the audio has played
 agent audio ◀──20 ms frames────┴─ Playout (paced, cancellable)
```

Each call is an independent actor: calls share only immutable business config, the
in-memory voice library, the HTTP connection pools and the TTS cache.

The session lives in `crates/callora-runtime/src/session.rs` (the actor and its events)
with three parts beside it: `session/hearing.rs` (caller audio, VAD, recognition, final
transcripts), `session/agent_turn.rs` (the agent's streamed decision) and
`session/speech.rs` (directives, speech planning, playout, hangup and transfer).

## 3. The agent (`crates/callora-core/src/agent.rs`)

**The request.** The system prompt is built from the business config and does not change
during a call, so the provider caches it: the tasks and their details, the slots, the
reply format, generic rules, the business's own rules (`agent.rules`), its examples
(`agent.prompt`) and its instant phrases. The per-turn message carries the conversation,
the known customer, the address form (masculine / feminine / unknown, when the business
configures `agent.prompt.address_forms`), the task with every slot's state, the step the
caller is on, notes from the engine about the previous turn's values, and the caller's
words (with a second, hinted transcription when there is one).

**The reply** is strict JSON, in this order, so the runtime can act on each part as it
streams in:

| Key | Meaning |
| --- | --- |
| `action` | `none`, `read_back`, `submit`, `transfer`, `end_call`. Known after a few tokens: a read-back, submit or goodbye holds the words for the engine. |
| `fields` | Values the caller gave, in their words. Checked against the parsers *before* a word plays: a value that will be rejected silences the reply, and the engine asks for it again. |
| `asks` | The details the turn's question asks for. A required detail asked for earlier and still missing stays open: a reply whose question asks only for other details is held before a word plays, and the engine asks for the open one again. |
| `phrase` | The id of a recorded phrase (`agent.phrases`), or null. It plays the moment the id is complete: no Hebrew text to generate, no TTS. |
| `read` | The agent's read of the caller, first in the reply and never spoken: `""` when the words mean what they say (most turns, about two tokens), else one terse English line (what they really mean, how they sound). The reply is then decided on it. |
| `tone` | neutral, friendly, joking, rushed, frustrated, sarcastic, confused or rude. Kept for the last six turns (`CallState.moods`) and told back to the agent ("CALLER'S TONE on their last turns"), so the mood of the call is remembered, not only that of one sentence. |
| `say` | Anything else to say, spoken sentence by sentence as it streams (a sentence that is a recording plays as its clip). Usually empty with a phrase. |
| `task` | The task the caller is on. |

**Listening.** The prompt's "HOW TO LISTEN AND ANSWER" is generic (meaning over words, use the whole
call, match the caller's tone and length, spoken language, no opening acknowledgements); the business
adds its own rules and examples (`agent.rules`). Beside the model's own read, the engine adds one
deterministic signal: when the caller's words nearly repeat an earlier turn of theirs
(`agent::repeats_earlier`, word overlap without Hebrew prefixes) the turn carries a REPEAT note, so the
agent owns the repetition and does not ask again. The cost is the `read` and `tone` keys: about two
tokens on a plain turn, up to ~25 when the agent has something to read. `agent.reading: false` in the
business file removes them. Measure first-words latency with `callora eval` (and `turn timing` on live
calls) before and after.

**The engine enforces** (`Engine::on_agent_turn`): values go through the same typed
parsers and the places list as everything else; a place the caller never said is refused;
`submit` runs only right after a confirmed read-back and a yes the caller really said (a
word recognition made up is not a yes); `end_call` ends only on a real goodbye; a detail
corrected after the read-back is read back again; a question asked five times in a row
goes to a person.

**The model.** Chat completions with strict JSON-schema output
(`crates/callora-providers/src/openai.rs`). Default `gemini-3.8-flash` (thinking `low`,
through Gemini's OpenAI-compatible endpoint), hedged by OpenAI's `gpt-6-luna`: if the primary has said nothing after
`AGENT_HEDGE_MS` (1200), the backup gets the same request and whichever speaks first is
used. `AGENT_MODEL`, `AGENT_BACKUP_MODEL` and `AGENT_REASONING_EFFORT` change them; run
the eval before doing so. Every reply's token counts are kept for the cost per call.

**When the agent fails** (an error or `agent.timeout_ms`), the rules take the turn: the
deterministic fast path, and the business's fallback ladder.

## 4. Understanding without the agent (§5, §6, §9, §25, §26)

A business without an `agent` block, and the fast lane, use the deterministic
understanding in `understanding.rs`: meta intents from per-business lexicons, yes/no only
in the opening words, business intents by keywords, slots in priority order with typed
parsers (Hebrew numbers, times, the places list, customer aliases, synonyms, bare answers
to the question just asked), and a coverage score. Below the business's threshold the
runtime asks an understanding LLM (`llm.rs`; Gemini and OpenAI raced when both are set)
and merges its answer with the rules'.

## 5. The engine (§6–§10, §20, §26–§28, §34)

- **Meta intents never destroy state.** Repeat and "didn't understand" replay the last
  plan. "Speak slower" and "louder" become sticky. "Wait" says nothing.
- **Slot filling** asks only for the next missing required slot and fills defaults and
  customer-known values. An `ask_before_confirm` slot (the note for the driver) is asked
  once before the first read-back.
- **An answer goes to its question.** A value for a detail already given, passed while
  the question was about another, is not taken unless the caller is correcting
  (`lexicon.correct`: "לא", "טעיתי", ...); the agent is told why.
- **Places** are checked against Israel's localities and streets (`gazetteer.rs`): a city
  alone for a precise slot notes the city and asks for the street; a street the city does
  not have is asked again once; a place that is neither is asked for its address once.
- **Confidence tiers**, **corrections**, **intent switches** (suspend and resume),
  **rules** ("more than 8 passengers → a person") and the **fallback ladder** (§28).
- **Actions** run with a run id that is the same on every attempt: the HTTP backend gets
  it as an idempotency key (`Idempotency-Key` header and `idempotency_key` in the body).
  A failure after the request may have arrived (a timeout) is an **unknown outcome**: the
  caller hears the pipeline's `on_unknown` ("רגע, אני מעביר למוקדן שיוודא שההזמנה
  נקלטה."), a person gets the call with the details, and the order card is marked to be
  checked. It is never reported as "failed", since the ride may be on its way.
- **Handoff** (§27) carries a summary (reason, task, details, recent turns). A Twilio
  `<Dial>` whisper reads it to the human before the caller is connected.

## 6. Audio (§11–§19)

- **The voice library** holds every fixed sentence and every template expansion ("הנהג
  יגיע בעוד {eta}" is 30 clips), content-addressed by delivery and text, built with
  `callora voice-library build`, loaded into memory.
- **Dynamic TTS** (ElevenLabs, `ulaw_8000`) streams into playout; a long sentence is split
  into short pieces synthesized side by side; finished syntheses go into an LRU cache. A
  reply that opens with live TTS starts with a recorded cover ("אממ, כן.").
- **Speech recognition**: Deepgram Nova-3 in Hebrew (`STT_PROVIDER=scribe` for ElevenLabs
  Scribe, `cartesia` for Cartesia), with the runtime's VAD deciding when an utterance
  ends (`endpointing=false`, then `Finalize`). Its
  keyterms are biased with the streets of a city while its street is the question (a
  second session opens beside the live one and takes over between utterances), and go
  back to the business's words once the question moves on: left on, a city's streets
  turned the caller's name into one of them. Keyterms stay within Deepgram's 500-token
  budget (900 characters); a session refused for them opens without them. A second, slower,
  hinted transcription (`SECOND_HEARING=1`) is off until measured.
- **Playout** sends 20 ms frames about 60 ms ahead of real time. Cancel drops everything
  and sends Twilio `clear`. Barge-in: energy VAD on the caller's track after ~100 ms.
- **Fillers**: the agent's thinking filler ("אממ...") if its first words are late, never
  on two turns in a row; a pipeline's own filler when its action starts.

## 7. Latency budget

| Step | Typical |
| --- | --- |
| Caller stops → VAD endpoint → STT final | `VAD_ENDPOINT_MS` of silence + the final transcript |
| Agent: request → phrase id | the model's time to its first ~20 tokens |
| Phrase id → first frame to Twilio | next frame (0–20 ms) |
| Free text → first frame | the first sentence of `say`, then TTS time-to-first-byte |
| Fast lane (engine only) | < 1 ms |
| Barge-in | ~100 ms detection + one frame |

Each turn logs `turn timing` (recognition, the agent's first words, the reply's first
frame and whether it was recorded or live). `/metrics` has
`callora_response_latency_ms`, `callora_llm_latency_ms`, `callora_barge_in_latency_ms`
and the audio-source mix `callora_audio_segments_total{source=...}`.

## 8. Quality: the eval and the calls page

- **`callora eval`** ([`evaluation/README.md`](../evaluation/README.md)) runs the agent,
  with the real model, on conversations that went wrong on live calls
  (`evaluation/agent/*.json`), several times each, through the same engine and mock
  actions, and reports the pass rate, first-words and decision latency, tokens and cost
  per turn, per model. It is how a prompt, rule or model change is judged.
- **The dashboard** (`web/`, served at `/`, signed in with `DASHBOARD_PASSWORD`) shows the numbers of the last day / week / month (calls, the
  share done without a person, handoffs, calls with nothing done, orders to check, the
  median time and caller turns to a completed task, cost per call, reviewed calls), every call with each caller turn's decision (route, latency,
  action, phrase, values), the caller's recorded utterances (sampled numbers only), a
  good / bad verdict with a note, and the export of a call as an eval case.

## 9. Persistence and observability

PostgreSQL (schema `callora_v2`, separate from the legacy tables) stores calls (with their
token usage), turns (with the agent's decision on each caller turn), action runs,
handoffs, order cards, reviews, and the final state. Writes go through a bounded queue,
so a slow or missing database never slows a call. Transcripts and recorded utterances are
deleted after `TRANSCRIPT_RETENTION_DAYS`. Logs are structured JSON per call.

## 10. Adding a business

Write `businesses/<id>.json` (copy the taxi file, or the clinic fixture for a small
start): its intents, pipelines, slots, responses, and an `agent` block with its persona,
rules, examples (`prompt`) and the responses offered as instant phrases. Run
`./dev callora config validate`, talk to it with `./dev simulate <id>`, write eval cases
for it, build its voice library, and set its phone numbers' environment variable. No code
changes are needed unless it needs a new *kind* of action backend.

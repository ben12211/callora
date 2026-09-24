# Callora V2 — Rebuild Plan

Source of truth for *what* Callora V2 is: [`callora_voice_agent_architecture.md`](callora_voice_agent_architecture.md).
This file records *how* we get there: ten major milestones, each with its objective,
the main work involved, and what counts as done.

The legacy Node/TypeScript implementation (a speech-to-speech bridge to OpenAI Realtime,
ElevenLabs Agents, Cartesia and Deepdub) is preserved unchanged on the `OLD-MAIN` branch
(`bb8d327`). V2 does not refactor it. It keeps only the integration knowledge
(see [`LEGACY_INVENTORY.md`](LEGACY_INVENTORY.md)).

---

## Task 1 — Legacy preservation and a clean Rust foundation

**Objective.** Freeze the old system, keep what it knows about the outside world, and start
a clean Rust codebase.

**Work.**
- `OLD-MAIN` branch at the exact current `main` commit.
- Inventory the legacy integrations: Twilio webhook paths, signature and stream-token
  scheme, ElevenLabs/Cartesia/OpenAI endpoints and models, Hebrew STT tuning, deploy and CI
  conventions, and the GitHub Secrets/Variables names.
- Remove the legacy application code from the V2 line. Keep the deploy tooling and the
  Hebrew evaluation corpus.
- Cargo workspace with its crate layout, lints, and a Rust CI job.
- `SECRETS.md` and `.env.example` listing every variable name. Names only, never values.

**Done when.** `OLD-MAIN` equals the old `main`; `cargo build && cargo test` pass on an empty
workspace skeleton; the secrets inventory exists and contains no values.

## Task 2 — Business configuration model and validation

**Objective.** Business behaviour is data. A validated Business JSON fully describes a
business: identity, language, voice personality, intents, meta-intent lexicon, pipelines,
slots, rules, actions, audio responses, pronunciations, fallbacks, and handoff.

**Work.** Typed `serde` schema with `deny_unknown_fields`; a semantic validator for
cross-references (pipelines → slots → responses → actions, compiled regexes, template
parameters); a loader for a directory of businesses keyed by phone number; the complete
taxi business config (`businesses/taxi.json`); a `callora config validate` command.

**Done when.** The taxi config loads and validates. Every class of broken reference is
rejected with a precise path, and tests cover it.

## Task 3 — Generic conversation engine (state, slots, meta-intents, pipelines)

**Objective.** A deterministic, provider-free engine that owns the call state. Raw LLM
history is never the source of truth for business state.

**Work.**
- `CallState`: active pipeline run, slot values with confidence and provenance, awaiting
  slot or confirmation, last response, fallback level, delivery mode, suspended pipelines,
  and an undo journal for `go_back`.
- Meta-intents (`repeat_last`, `did_not_understand`, `speak_slower`, `speak_louder`,
  `cancel_current_flow`, `go_back`, `transfer_human`, `goodbye`) that never destroy
  collected state.
- Slot filling that asks only for what is missing and accepts many slots in one utterance.
- Confidence tiers (proceed / confirm / clarify), per-action confirmation requirements,
  business rules, the three-step fallback ladder, and handoff triggers.
- The engine emits `Directive`s (respond, run action, hand off, hang up). It never does I/O.

**Done when.** Scripted conversations from the MD (§29–31, meta-intent mid-pipeline,
correction mid-confirmation, fallback ladder) pass as unit tests.

## Task 4 — Understanding layer (meta + business intent, slot extraction)

**Objective.** Turn an utterance into `Understanding { meta_intent?, intent?, slots[], confidence }`
fast.

**Work.** A deterministic fast path driven by config: meta-intent and yes/no lexicons, intent
keywords, per-slot regex patterns, Hebrew number words, a time parser ("now", "in 10
minutes", "08:30"), a places gazetteer with aliases, and slot answers interpreted in the
context of the question just asked. An LLM structured-extraction path (OpenAI-compatible
JSON schema, generated from the business config and the current state) runs only when the
fast path is not confident, under a hard timeout. An arbiter merges the two.

**Done when.** The MD example utterances produce the expected intents and slots through the
fast path. LLM extraction is exercised with a mocked HTTP server. A timeout degrades to a
clarification, never to silence.

## Task 5 — Response planner, text normalization, pronunciation

**Objective.** Every reply is a plan made of audio segments. Cached audio is preferred over
template audio, and template audio over dynamic TTS.

**Work.** Response catalogue with variants (no immediate repetition), templates with typed
parameters (Hebrew number agreement for masculine and feminine counts), rendering to spoken
text; a Hebrew normalizer (₪, %, phone numbers, times, dates, digits → words); a per-business
pronunciation dictionary. The planner resolves each segment to `Cached(clip)` or `Tts(text)`
and splits "ack + dynamic remainder" so the cached acknowledgement covers TTS latency.

**Done when.** Planner tests show the MD's replies resolving to cached, template, or TTS
segments as expected, and a synthetic taxi conversation stays mostly cached.

## Task 6 — Audio system: library, dynamic TTS, playout with cancellation

**Objective.** Cached audio starts playing within one frame. Playback is cancellable at any
frame boundary.

**Work.** μ-law codec and energy VAD; an on-disk voice library (content-addressed `.ulaw`
clips plus a manifest) loaded into memory at startup; `callora voice-library build` to
pre-generate every cached variant and every template expansion through ElevenLabs
(`ulaw_8000`); a streaming ElevenLabs TTS client for dynamic segments, with an LRU cache; a
playout engine that paces 20 ms frames with a small lead, supports generation-tagged
cancellation (barge-in), and reports marks so the runtime knows what was actually heard.

**Done when.** Tests show a cached clip's first frame emitted immediately, cancellation
stopping output within one frame, TTS segments streaming in order after cached segments,
and library build/load round-tripping against a mocked ElevenLabs.

## Task 7 — Telephony and the per-call realtime runtime

**Objective.** Real phone calls. Twilio → greeting → STT → understanding → engine → audio,
with first-class barge-in, on many independent concurrent calls.

**Work.** An `axum` server keeping the legacy webhook paths (`/webhooks/twilio/voice`,
`/call-status`, `/media`); Twilio signature validation; an HMAC stream token bound to the
CallSid and business; one actor task per call joining the Twilio media socket, the streaming
STT (Cartesia `ink-whisper`, μ-law 8 kHz, 0.5 s max silence), the understanding tasks, and
playout, all without blocking one another. VAD barge-in sends a Twilio `clear` and cancels
playout, TTS, and pending LLM work. The greeting plays from cache on `start`. Fillers play
while slow work runs. Silence handling and hangup via Twilio REST.

**Done when.** An end-to-end test drives the real server over WebSocket with a fake Twilio
client and a scripted STT: greeting audio arrives, a booking completes, and a barge-in during
a reply produces a `clear` and stops the frames.

## Task 8 — Business actions, tools, customer context, human handoff

**Objective.** Pipelines execute real business actions, and handoff carries the context.

**Work.** An action registry with backends: `http` (POST JSON to a business endpoint named by
an env var, with timeout, retries, and a result mapping), `mock` (config-declared results for
demos and tests), and `system` (transfer, hang up). Taxi actions: `create_ride`,
`cancel_ride`, `get_ride_status`, `get_driver_eta`, `estimate_price`, `find_customer`. The
customer lookup runs in parallel with the greeting, and context aliases ("מהבית") resolve
against the customer record. Handoff: a Twilio `<Dial>` to the business's handoff number,
with a whisper that reads the collected context to the human agent, and the context
persisted.

**Done when.** Tests cover an action success → result response, failure → retry → handoff, a
context alias resolving to the saved home address, and the handoff TwiML plus summary.

## Task 9 — Persistence, observability, simulator, admin API

**Objective.** Operators can see what happened and prove the latency targets.

**Work.** PostgreSQL through `sqlx` with embedded migrations (calls, turns, state
snapshots, action runs, handoffs); the runtime keeps working when the database is down.
Structured JSON tracing per call; a Prometheus `/metrics` endpoint with
speech-end → first-audio and barge-in → silence histograms and an audio-source mix
(cached/template/TTS); `/health`; a read-only admin API behind `ADMIN_API_KEY`; and
`callora simulate`, a text REPL over the real engine, planner and mock actions.

**Done when.** Migrations apply to a real Postgres (in CI), metrics render, and the
simulator runs the MD's example calls.

## Task 10 — Deployment, CI/CD, documentation, end-to-end hardening

**Objective.** Ship it with the same operational guarantees the legacy stack had.

**Work.** A multi-stage Dockerfile that cross-compiles for `linux/arm64` (cargo-zigbuild)
into a distroless non-root image with a built-in healthcheck subcommand; compose files;
`deploy.sh` adapted (`callora migrate`, a named `KEY=VALUE` secret sync instead of
positional lines); a CI workflow (fmt, clippy, test with Postgres, config validation,
image, deploy) keeping `DOCKER_HUB_*`, `IP`, `USER`, `KEY_PEM` and the `production`
environment; README, ARCHITECTURE, and an operator runbook for generating the voice library.

**Done when.** CI is green, the image builds, `docker compose config` validates, docs match
the code, and the full test suite passes.

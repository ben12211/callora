# Callora V2 architecture

This document explains how the [product architecture](callora_voice_agent_architecture.md)
is implemented. Section numbers (§) refer to that document.

## 1. Principles

1. **The engine is generic; businesses are data.** `callora-core` knows about intents,
   pipelines, slots, rules, actions and responses, and nothing about taxis. Taxi behaviour
   lives in `businesses/taxi.json` (§21–22).
2. **State is explicit.** `CallState` holds the active pipeline run, each slot's value with
   its confidence and provenance, what was just asked, whether a read-back is pending,
   suspended flows, and what was said last. The LLM never owns state. It only proposes
   values, which go through the same typed parsers as the rules (§8).
3. **Decisions are pure; I/O happens elsewhere.** The engine turns understanding into
   `Directive`s (speak, run action, hand off, hang up). The runtime executes them, so every
   conversational behaviour is a unit test (see `crates/callora-core/tests/taxi.rs`).
4. **Latency is a design constraint, not a tuning step** (§2.2, §33). Nothing on a call's
   hot path waits on the network unless it has to, and when it has to, the caller hears
   something (a cached acknowledgement or a filler).

## 2. A call, end to end

```text
Twilio ──POST /voice (signed)──▶ route by To → business
        │                        start customer lookup (in parallel)
        │◀── <Connect><Stream> + call-bound HMAC token
        │
        ├──WS /media start──▶ Session actor (one tokio task per call)
        │                       ├─ greeting from the voice library: plays on the next frame
        │                       ├─ STT connects in the background (audio is buffered)
 caller audio ─────────────────▶├─ VAD: speech start → barge-in (cancel + Twilio clear)
        │                       │       speech end   → STT finalize (fast endpoint)
        │                       ├─ STT final → fast-path understanding (µs)
        │                       │     └─ unsure? → LLM with a timeout; filler if slow
        │                       ├─ Engine → directives
        │                       │     ├─ Speak  → planner → library clip | TTS stream
        │                       │     ├─ Action → business backend (async) → result → Engine
        │                       │     └─ Hangup / Handoff → after the audio has played
 agent audio ◀──20 ms frames────┴─ Playout (paced, cancellable)
```

Each call is an independent actor: calls share only immutable business config, the
in-memory voice library, the HTTP connection pools and the TTS cache. Concurrency is
limited by CPU and provider quotas, not by shared locks.

## 3. Understanding (§5, §6, §9, §25, §26)

`understanding::fast_path` is deterministic and config-driven:

- **Meta intents** from per-business lexicons: `exact` phrases must be the whole utterance
  ("מה?"), while `phrases` may appear anywhere ("תחזור על זה").
- **Yes/no** only in the opening words, so a "לא" inside an address is not a refusal.
- **Business intents** by keyword hits. Ties go to the keyword mentioned first ("השארתי תיק
  במונית" is a lost item, not a booking).
- **Slots** are extracted in priority order. Patterns run on the real text, so they can
  anchor on keywords, but a value is cut at the first word something else already
  explained, and a value that starts on an explained word is skipped (so "מונית" is never
  read as "from ונית"). Typed parsers handle Hebrew number words, times ("עכשיו", "בעוד
  רבע שעה", "בשמונה וחצי"), the places gazetteer with aliases, customer aliases ("מהבית"),
  synonyms ("לבד" → 1), and bare answers to the question just asked.
- **Coverage** is the share of the utterance the rules explained. Below the business's
  threshold, the runtime asks the LLM.

`llm::build_request` builds a strict JSON-schema extraction request from the business
config and the live state. `llm::parse_response` validates the reply, and `merge` keeps
high-confidence rule findings while letting the LLM fill gaps. An LLM timeout falls back
to the fast path; it is never silence.

## 4. The engine (§6–§10, §20, §26–§28, §34)

- **Meta intents never destroy state.** Repeat and "didn't understand" replay the last plan
  exactly (slower for "didn't understand"). "Speak slower" and "louder" become sticky.
  "Cancel" drops only the current flow. "Go back" undoes the last slot change from a
  journal. "Wait" says nothing.
- **Slot filling** asks only for the next missing required slot, acknowledges new
  information ("סגור. לאן נוסעים?"), and fills defaults and customer-known values.
- **Confidence tiers:** below `reject_below` a value is ignored, below `confirm_below` it
  is read back on its own ("ז'בוטינסקי 5, נכון?"), and actions can require a minimum
  confidence or an explicit read-back (`requires_confirmation`).
- **Corrections** during the read-back ("לא, לעזריאלי") update the value and re-confirm.
- **Intent switches** mid-flow suspend the current run and resume it afterwards.
  Answer-only intents (FAQ) respond and then re-ask the pending question. Values given
  earlier in the call carry into a new flow ("how much to the airport?" … "ok, book it").
- **Rules** (for example "more than 4 passengers → van", "more than 8 → human") fire when
  slots change.
- **Fallback ladder** (§28): the business's escalating responses, then a handoff. Inside a
  flow the pending question is re-asked, so the caller never loses their place.
- **Handoff** (§27) carries a summary (reason, intent, collected slots, recent turns). A
  Twilio `<Dial>` whisper reads it to the human before the caller is connected.

## 5. Audio (§11–§19)

- **Plans and segments.** A reply is a list of segments. The runtime plays each from the
  **voice library** when that exact sentence in that delivery was pre-generated, and
  otherwise synthesizes it with **dynamic TTS**. A response's `prefix` ("סגור.") is its
  own segment, so the cached acknowledgement starts playing while the dynamic remainder is
  being synthesized.
- **Templates** with finite parameters (counted nouns with Hebrew gender agreement, bare
  numbers, enums) are expanded at build time: "הנהג יגיע בעוד {eta}" is 30 pre-generated
  sentences, not TTS. For the taxi business, 718 clips cover every static sentence and
  every template expansion, plus slow variants for repeats.
- **The library** is content-addressed (`delivery + text → clip`), built incrementally with
  `callora voice-library build`, loaded fully into memory, and ignored if it was generated
  for another voice.
- **Dynamic TTS** (ElevenLabs, `ulaw_8000`) streams into playout as chunks arrive. Finished
  syntheses go into an LRU cache, so "what?" after a dynamic sentence replays instantly.
- **Spoken text normalization** (§18) turns money, percentages, phone numbers,
  identifiers, times, dates and every remaining number into words. The per-business
  pronunciation dictionary (§17) is compiled once.
- **Playout** sends 20 ms frames about 60 ms ahead of real time. Cancel drops everything
  and sends Twilio `clear`, so at most about 3 frames were ever in flight. "Idle" means
  the reply has actually been heard, which is when hangups and transfers happen.
- **Barge-in** (§10, §33): energy VAD on the caller-only track triggers after about 100 ms
  of speech and cancels immediately. A transcript arriving while the agent talks also
  counts as a barge-in.
- **Fillers** (§19): pipelines play a cached filler when their action starts, and the
  runtime plays a "thinking" filler if the LLM is slower than the business's threshold.

## 6. Latency budget

| Step | Typical |
| --- | --- |
| Caller stops → VAD endpoint → STT `finalize` | 400 ms silence (configurable `VAD_ENDPOINT_MS`) + final transcript |
| Fast-path understanding + engine + plan | < 1 ms |
| Cached clip → first frame to Twilio | next frame (0–20 ms) |
| Dynamic segment | ElevenLabs time-to-first-byte, masked by the cached acknowledgement |
| Barge-in | ~100 ms detection + one frame |

`/metrics` measures `callora_response_latency_ms` (caller speech end → first reply frame)
and `callora_barge_in_latency_ms` directly, plus the audio-source mix
(`callora_audio_segments_total{source=cached|template|tts|tts_cached}`) against the MD's
70–80 / 15–20 / 5–10 target.

## 7. Persistence and observability

PostgreSQL (schema `callora_v2`, separate from the legacy tables) stores calls, turns
(with the understanding that produced each state change), action runs, handoffs, and the
final state. Writes go through a bounded queue, so a slow or missing database never slows
a call. Transcripts are pruned after `TRANSCRIPT_RETENTION_DAYS`. Logs are structured JSON
per call.

## 8. Adding a business

Write `businesses/<id>.json` (copy the taxi file). Run `./dev callora config validate`,
talk to it with `./dev simulate <id>`, build its voice library, and set its phone numbers'
environment variable. No code changes are needed unless it needs a new *kind* of action
backend.

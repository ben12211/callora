# Callora V2 — status

Plan: [V2_PLAN.md](V2_PLAN.md). This file records what is done, what was verified, and
what is still open. Last updated 2026-09-27.

## Where it stands

V2 is on `main` and deployed; the legacy implementation is preserved on `OLD-MAIN`
(`bb8d327`, verified on the remote). The taxi business takes live calls with an LLM agent
running the conversation (since 2026-09-25), recognized by ElevenLabs Scribe, with the
fixed questions from the voice library.

## Open items

| Item | State |
| --- | --- |
| **Measure the new agent model** | The default moved from `gpt-4o` to `gemini-3.8-flash` (thinking `low`, hedged by `gpt-6-luna`) for price. Not yet measured on the eval: no model key in the build environment. Run the **Agent eval** workflow (or `./dev callora eval --model gemini-3.8-flash --model gpt-4o`) and check first-words latency on live calls (`turn timing` in the logs) before relying on it. |
| **Places one step at a time** | From the owner: never mix up the pickup and the destination. The agent asks the pickup before the destination, one place and one step per question (city, street, house number); the house number is asked on its own, once. A street its city lacks is said back ("לא מצאתי את X בעיר. התכוונת ל…? / אולי זה בעיר אחרת?"); insisting takes it as said, marked for the driver; a third miss goes to a person, or, with no desk (`TAXI_HANDOFF_NUMBER` is not set today), is taken as said. A place that is no locality ("אפרק") is not taken; a new city for a place drops its old value; "כן אבל…" is no yes. |
| **WhatsApp** | `whatsapp/` (whatsapp-web.js, one Chromium per account, up to 5) and the dashboard's WhatsApp page: accounts signed in by QR, a list of targets per account (only groups it is in and saved contacts, checked again before each message), and a pace per account (random 8–15 s between messages, "typing…", 60 an hour and 400 a day by default, quiet hours, half for the first 3 days of a number). Orders and handoffs queue in `whatsapp_outbox` with the order and are retried until sent. Not yet run against a real WhatsApp account. |
| **Orders to Telegram groups** | A Telegram account (the "טלגרם" tab) now has the same "תפוצה" as a WhatsApp one: groups, channels and contacts picked from its own chats, per-target events, pace, queue and log. The sender drains `whatsapp_outbox` for WhatsApp and Telegram accounts alike (`Service::all_sessions`). Not yet run against a real Telegram group: send a test message from the tab first. |
| **The dashboard** | `web/` (React, TypeScript, Tailwind), served by the server at `/` in place of the `/calls` and `/orders` pages: overview with the numbers and calls per day, calls with each turn's decision, recordings and review, orders. Password login (`DASHBOARD_PASSWORD`, default `12345678` until set) with a signed session cookie. Settings (stage 2) are not built yet. |
| **Shorter silences** | Measured on the day's calls: from the end of the caller's speech to the reply, ~0.5 s VAD endpoint + ~0.36 s final transcript + ~1.1 s to the agent's first words (median ~2 s). The agent now starts on the partial transcript (`AGENT_SPECULATE` on by default, ~0.35 s); words dropped as noise are followed after 2 s by the question again ("כמה נוסעים?", the owner wanted no "say it again?") instead of the silence reprompt; the reprompt is at 5 s instead of 7. Watch `turn timing` for the effect. |
| **Side talk and broken-off words** | The line itself is quiet (noise floor about -72 dBFS): what reached the agent as answers was the caller talking to someone else, at the same loudness and recognizer confidence as real answers, so no noise filter separates them. The agent is told that speech not meant for it is not an answer. A last word that is a lone letter ("מ") waits for the rest of the sentence, as Scribe's "..." did. |
| **Deepgram vs Scribe on a live call** | The owner's call of 2026-09-28 (CAb65e03): "מבן זכאי 45 אלעד לירושלים" came out "לאילת באמת זכאי 45 לירושלים" in Deepgram; Scribe with the call's hints heard "מאלעד" but garbled the street ("מבית דקיי"). With no street hints Deepgram heard the name ("בניהו") right twice, where Scribe did not; the live call's "בן איוב" came from Eilat's streets left on as hints. Deepgram stays the default; streets now bias only while asked. Compare more calls before deciding (`STT_PROVIDER=scribe` switches). |
| **No moving on past an unanswered question** | The agent says what its question asks for (`asks`); a required detail asked for and not given stays open, and a reply that asks for something else instead is held and the engine asks for the open detail again (the street by its own question). Held turns show on `/calls` as "עבר לשאלה אחרת בלי תשובה". Not yet run on the eval with the real model. |
| **A goal instead of a question order** | The agent is told what a ride needs and to get it in as few turns as possible: it opens with "מאיפה לאן?" (`ask_route`) and may ask for two missing details in one question; the driver's note and the name are still asked before the read-back. Not yet measured: run the eval, then watch "זמן עד הזמנה" (median seconds and caller turns to the first completed task) on `/calls`. |
| **Deepgram Nova-3 hears the caller** | Speech recognition moved from ElevenLabs Scribe to Deepgram Nova-3 (Hebrew, keyterms within its 500-token budget, the runtime's VAD ends each utterance). Tested against its message format, not yet on live calls: listen to the owner's test calls on `/calls` and compare with Scribe (`STT_PROVIDER=scribe` switches back). |
| Recorded phrases by id | The agent now names a recording (`phrase`) instead of writing its words. Verified in tests with a scripted model; watch the audio-source mix (`callora_audio_segments_total`) on live calls: recorded should rise, live TTS fall. |
| New voice library clips | `ask_place_address` and `ride_unconfirmed` were added: run `voice-library build` (they play through live TTS until then). |
| Dispatch backend idempotency | Callora sends the key; whether `TAXI_DISPATCH_URL` honours it is up to that backend. `create_ride` keeps `max_attempts: 1` until it does. |
| Eval cases | 37 cases from the bugs seen on live calls. Their expectations were checked against the engine and the places list, not yet against a model run: the first run may show cases whose wording needs loosening. |
| Second hearing | Off (`SECOND_HEARING`) until measured: with a city's streets as hints it made names up on live calls. |
| CI gate before deploy | `quality` runs on pull requests only; a push to `main` deploys without it (a deliberate choice for now). |

## Done

| Area | Evidence |
| --- | --- |
| Legacy preservation, Rust foundation | `OLD-MAIN`, `docs/LEGACY_INVENTORY.md`, Cargo workspace, `./dev` |
| Business configuration and validation | `config.rs`, `business.rs`, `businesses/taxi.json`, `callora config validate` |
| Generic engine | `engine.rs`, `state.rs`; scenario tests in `crates/callora-core/tests/taxi.rs`; a second business (`tests/fixtures/clinic.json`) runs through it in `tests/second_business.rs` with no taxi words in its prompt |
| The agent | `agent.rs` (prompt from config, strict reply schema, phrase ids), `session/agent_turn.rs` (streamed: values checked before a word plays, a phrase plays on its id), `Engine::on_agent_turn` (the rules) |
| Agent eval | `callora eval`, `evaluation/agent/*.json`, `evaluation/README.md`, the **Agent eval** workflow |
| Understanding without an agent | `understanding.rs`, `llm.rs`, `time.rs`, `hebrew.rs` |
| Places | Israel's localities, streets and places (`gazetteer.rs`, `data/`), Ashkenazi pronunciations, streets biased per city in recognition |
| Response planning, audio | `render.rs`, `speech.rs`, `callora-audio`; playout/VAD tests |
| Telephony and per-call runtime | `session.rs` + `session/`, `server.rs`; full-call WebSocket tests |
| Actions, customers, handoff | `actions.rs` (idempotency key, unknown outcome), customer lookup at webhook time, `<Dial>` + whisper |
| Persistence, observability, owner's pages | `store.rs` + migrations (tested on Postgres), `/metrics`, `/api`, `/orders`, `/calls` (numbers, decisions, recordings, reviews, eval export), token usage and cost per call |
| Deployment, CI/CD | Dockerfile (arm64), `docker-compose.prod.yml`, `deploy.sh`, `ci-cd.yml`, `agent-eval.yml` |

## Verification commands

```bash
./dev check                          # fmt + clippy -D warnings + all tests (Postgres included)
./dev callora config validate
./dev callora eval --check           # the eval cases are well formed
./dev callora eval                   # the agent on them, with the real model
./dev simulate taxi
```

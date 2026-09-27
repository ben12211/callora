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
| **Measure the new agent model** | The default moved from `gpt-4o` to `gpt-6-sol` (reasoning `none`, hedged by `gpt-6-luna`) because callers got plainly wrong answers. Not yet measured on the eval: no model key in the build environment. Run the **Agent eval** workflow (or `./dev callora eval --model gpt-6-sol --model gpt-4o`) and check first-words latency on live calls (`turn timing` in the logs) before relying on it. |
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

# Callora V2 — status

Plan: [V2_PLAN.md](V2_PLAN.md). This file records what is done, what was verified, and
what is still open.

## Open items (unresolved)

| Item | State |
| --- | --- |
| **`OLD-MAIN` on the remote** | **UNRESOLVED.** The branch exists locally at `bb8d327` (the exact legacy `main`), but pushing it to GitHub was refused (HTTP 403: the Claude GitHub App has no write access to `ben12211/callora`). It stays unresolved until `git ls-remote origin refs/heads/OLD-MAIN` shows `bb8d327ab799171b54968e849a0f52a155044988`. Meanwhile the remote `main` still points at that commit, so the legacy code has not been touched. |
| Pushing the V2 branch | Blocked by the same 403. The V2 work is committed locally on `claude/bold-faraday-c8z1mm`. |
| VM bootstrap and cleanup | Automated in CI (`host` job: bootstrap, `init-host`, optional guarded `reset` that purges the legacy stack). Scripts were exercised locally against a real Docker daemon, but **not yet run on the VM**: this build environment has neither the SSH key nor outbound SSH, and the workflow cannot run until the branch is pushed. |
| First production cutover | Requires GitHub access, then the GitHub Secrets/Variables in [SECRETS.md](../SECRETS.md) and a voice library build (see DEPLOYMENT.md). |
| Live-provider validation | ElevenLabs, Cartesia, OpenAI and Twilio adapters are verified against local mock servers and recorded wire formats, not yet against the live services (there are no credentials in the build environment). Worth checking on first deploy: Cartesia `finalize` → `flush_done` behaviour with `ink-whisper`, and ElevenLabs `eleven_v3` streaming latency for dynamic segments (`ELEVENLABS_DYNAMIC_MODEL` can switch the model). |

## The ten tasks

| # | Task | State | Evidence |
| --- | --- | --- | --- |
| 1 | Legacy preservation and a clean Rust foundation | Done locally; **`OLD-MAIN` push unresolved** | `docs/LEGACY_INVENTORY.md`, `SECRETS.md`, Cargo workspace, `./dev` |
| 2 | Business configuration model and validation | Done | `config.rs`, `business.rs`, `businesses/taxi.json`, `callora config validate`, validation tests |
| 3 | Generic conversation engine | Done | `engine.rs`, `state.rs`; 20 scenario tests incl. MD §29–31 |
| 4 | Understanding layer | Done | `understanding.rs`, `llm.rs`, `time.rs`, `hebrew.rs`; LLM tested via a mock server |
| 5 | Response planner, normalization, pronunciation | Done | `render.rs`, `speech.rs`; legacy 110-utterance corpus as a test |
| 6 | Audio system | Done | `callora-audio`; playout/VAD tests; library build/load against mocked ElevenLabs |
| 7 | Telephony and per-call runtime | Done | `session.rs`, `server.rs`; full-call WebSocket test (greeting, booking, barge-in + clear, action, hangup) |
| 8 | Actions, customer context, handoff | Done | `actions.rs`, customer lookup started at webhook time, `<Dial>` + whisper, handoff summary tests |
| 9 | Persistence, observability, simulator, admin API | Done | `store.rs` + migration (tested on Postgres), `/metrics`, `/api`, `callora simulate` |
| 10 | Deployment, CI/CD, docs | Done locally | Dockerfile (arm64 cross-build verified, amd64 image run and healthy), `docker-compose.prod.yml`, `deploy.sh` (`update-secrets` exercised in a container), workflow, README/ARCHITECTURE/DEPLOYMENT |

## Verification commands

```bash
./dev check                          # fmt + clippy -D warnings + all tests (Postgres included)
./dev callora config validate
./dev simulate taxi
docker buildx build --platform linux/arm64 .
```

## Next steps after cutover

- Record real latency percentiles from `/metrics` and tune `VAD_ENDPOINT_MS` and the LLM
  coverage threshold per business.
- A management UI on top of `/api` (the legacy dashboard was not carried over).
- More businesses (clinic, restaurant) as JSON files: the engine needs no changes.

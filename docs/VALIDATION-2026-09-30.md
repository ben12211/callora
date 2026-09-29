# Handoff and speech recovery validation — 2026-09-30

Work continued in the existing worktree `.claude/worktrees/sad-zhukovsky-4c26c5`, branch `claude/project-strengths-weaknesses-6b2173`, based on `2690b71`. The root checkout was an older `main`; its files and other worktrees were left intact.

## Bugs and fixes

Previously unfinished changes retained:
- Barge-in now counts actual 8 kHz μ-law audio duration; short noise and read-back backchannels do not interrupt. A yes after an interrupted read-back replays the read-back rather than submitting.
- A continuation can belong to the previous answer even when no previous pause is known; long known gaps remain excluded. Interrupted, already spoken agent words are remembered.
- Speech with no final transcript gets recovery instead of permanent silence.
- Noise is explained briefly, once per turn, followed by the pending question; “hello?” is answered. Wait requests receive an acknowledgement and a longer wait. Short expected answers are accepted, thanks after “anything else?” closes, and callers with details already supplied get patient reprompts.
- Cross-provider fallback after the primary/hedge fail before output is retained and tested.

Additional defects found and corrected:
- An operator answering while another call was being created could leave the late leg ringing. Winner reservation is atomic; the late leg is canceled, remaining numbers are not dialed, duplicate destinations are skipped, and duplicate transfer requests do not create another batch.
- The hold deadline started after dialing, and Twilio requests had no overall timeout. The deadline now starts when the caller is put on hold and includes call creation and whisper time. REST requests have a ten-second timeout.
- “Answered” meant only that Twilio fetched the whisper, so hanging up before joining could leave hold music indefinitely. Signed conference start/end callbacks now confirm actual joining and clean up. A hangup before joining activates fallback, and an answer without a join remains bounded by the deadline.
- A repeated answer webhook could hang up the winning leg. The same winner receives the same join TwiML; another leg is refused.
- Caller hangup did not stop desk legs. Signed caller status and conference end callbacks now cancel them and invalidate late answers.
- Failure to create every leg left transfer state open; failed fallback redirects were silently ignored. Terminal state is set once, fallback is immediate, and a failed redirect attempts hangup with internal error logs.
- A completed leg that never joined was ignored. Terminal statuses, including defensive rejected handling, are covered without disturbing an already connected handoff.
- The watchdog could fire during partial ASR progress, or lose recovery while another response was busy. Partial progress renews the two-second grace; generation checks and task cancellation invalidate stale timers on new speech, final words, and termination. Busy recovery retries after a grace period. Silence reprompts now also guard speech, pending agent work, second hearing and unfinished transcripts.
- Broad “harmless” denial stripping could turn “כן, לא צריך מונית” into booking confirmation. Only complete non-correcting phrases are exempted; explicit cancellations never submit.
- Some configured phrases contradicted the agent instructions. Generic fallback/restart and city clarification were updated; customer responses stay short and gender-neutral. Agent action/field JSON and booking/handoff guards remain intact.
- The new patient response was not validated as a configured response; it now is.
- Outbound caller ID was fixed to the incoming business number. Optional per-business `caller_id` is now validated, saved and editable on the dashboard; old settings default to the business number.
- Twilio creation errors could include raw private destination details. Logs keep HTTP/error codes instead, along with destination index, call SID, terminal status and fallback activation.
- `AGENT_FALLBACK_MODEL` was absent from deployment propagation. It is now in the example environment, Compose, deployment allowlist and workflow.
- Windows checkout converted the extensionless `dev` shell wrapper to CRLF. An LF attribute now keeps it usable in the Docker workflow.

Validation environment issues: the shared Cargo volume contained artifacts from another checkout, and the shared development database had a different historical migration checksum. Project-only build artifacts were cleared and a new validation database was used. Existing database contents were preserved. Docker Desktop stopped during the first run; its existing installation was restarted.

No e6.py/e7.py/e10.py helpers were present in this worktree. No temporary debugging or test-output files are committed.

## Regression coverage

- Core taxi suite: 93 passing tests, including expected short answers, interruption/read-back guards, harmless confirmation versus cancellation, continuation context, noise explanation, closing, hello and patient silence.
- Runtime unit suite: 29 passing tests. Desk tests cover each terminal status, all destinations failing, immediate and exactly-once fallback, create/redirect failures, configurable caller ID, duplicate transfer requests, caller termination, timeout, answer races during creation, a late leg after deadline, whisper abandonment, repeated answer callbacks and confirmed conference success.
- WebSocket call-flow suite: 15 passing tests, including a complete booking, real frame-duration barge-in, interrupted read-back recovery, unknown previous-pause continuation, no-words recovery, partial ASR grace, final transcript cancellation, new utterance invalidation and signature checks over token plus form for every handoff callback.
- Provider integration suite: 5 passing tests, including Twilio POST completion callbacks, outbound caller ID and sanitized error responses. Provider unit suite: 29 tests, including healthy/failing fallback extract and stream behavior.
- Database history/review test: passed against real Postgres; action integration tests: 3 passed. Other business, Hebrew normalization, audio, CLI and eval-engine tests all pass.
- Three existing taxi wording assertions were updated to intentionally changed phrases; their slot, action and fallback-state checks were preserved. The noise eval expectation is stronger, requiring the intended explanation instead of merely forbidding old wording.

## Final results

234 Rust tests passed, 0 failed, 0 ignored; workspace doc tests passed. Formatting and strict Clippy passed. All 42 agent case files passed format validation. Business configuration, dashboard typecheck/build, WhatsApp TypeScript build/JavaScript syntax, Python AST syntax, shell syntax, GitHub Actions syntax, both Compose configurations and git diff whitespace checks passed.

The dashboard build retains a nonfatal large-chunk warning. npm reported pre-existing dependency deprecation notices in WhatsApp. Neither Node package defines a lint/test script, and Python has no configured lint/typecheck suite.

## Commands run

All toolchains/dependencies ran in containers, with no host installation. Commands below were run from the identified worktree; repeated fmt/clippy/test runs followed corrections.

```powershell
git status --short
git branch -avv
git worktree list
git diff --stat
git diff
git log --oneline
git fetch origin
git rev-list --left-right --count HEAD...origin/main
git diff --check

docker compose run --rm toolchain cargo fmt --all -- --check
docker compose run --rm toolchain bash -c 'cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace'
docker compose run --rm toolchain bash -c 'cargo clean -p callora-core -p callora-audio -p callora-runtime -p callora-providers -p callora && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace'
docker compose run --rm toolchain bash -c 'cargo test -p callora-runtime --test store > target/store-check.log 2>&1; result=$?; tail -85 target/store-check.log; exit $result'
docker compose exec -T db psql -U callora -d postgres -c 'CREATE DATABASE callora_validation_20260930'

docker compose run --rm -e TEST_DATABASE_URL=postgresql://callora:callora@db:5432/callora_validation_20260930 toolchain bash -c 'cargo fmt --all && cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings > target/clippy-validation.log 2>&1 && cargo test --workspace > target/test-validation.log 2>&1; result=$?; tail -12 target/clippy-validation.log; grep -E "test result:|FAILED|panicked" target/test-validation.log; exit $result'

docker compose run --rm toolchain bash -c 'cargo run -q --bin callora -- eval --check && cargo run -q --bin callora -- config validate && bash -n dev deploy/deploy.sh deploy/bootstrap-oracle-linux.sh deploy/ci-ssh-setup.sh && python3 -c "import ast,pathlib; ast.parse(pathlib.Path(\"data/build_places.py\").read_text())"'
docker compose run --rm toolchain bash -c 'cargo run -q --bin callora -- eval --repeat 1 --json target/agent-eval-validation.json'

docker run --rm --mount ('type=bind,src=' + (Get-Location).Path + ',dst=/workspace') --mount type=volume,src=callora-node-web-check,dst=/workspace/web/node_modules --mount type=volume,src=callora-node-web-dist,dst=/workspace/web/dist -w /workspace/web node:24-bookworm-slim bash -c 'npm ci --no-audit --no-fund && npm run typecheck && npm run build'
docker run --rm --mount ('type=bind,src=' + (Get-Location).Path + ',dst=/workspace') --mount type=volume,src=callora-node-whatsapp-check,dst=/workspace/whatsapp/node_modules --mount type=volume,src=callora-node-whatsapp-dist,dst=/workspace/whatsapp/dist -w /workspace/whatsapp -e PUPPETEER_SKIP_DOWNLOAD=true node:24-bookworm-slim bash -c 'npm ci --no-audit --no-fund && npm run build && node --check dist/server.js'
docker run --rm --mount ('type=bind,src=' + (Get-Location).Path + ',dst=/workspace') -w /workspace rhysd/actionlint:latest -shellcheck= -pyflakes= .github/workflows/ci-cd.yml
docker compose config -q
# Production config validation used dummy, nonsecret values:
$env:CALLORA_IMAGE='validation/callora:test'
$env:POSTGRES_USER='callora'
$env:POSTGRES_PASSWORD='validation-only'
$env:POSTGRES_DB='callora'
$env:DATABASE_URL='postgresql://callora:validation-only@db:5432/callora'
$env:PUBLIC_BASE_URL='https://callora.example.invalid'
$env:TWILIO_ACCOUNT_SID='AC00000000000000000000000000000000'
$env:TWILIO_AUTH_TOKEN='validation-only'
docker compose -f docker-compose.prod.yml config -q
```

## Unresolved external verification

The live-model eval command exited with `the agent model's API key is not set`. Its 42 cases were validated, but no live-model pass rate can be claimed. Live Twilio/carrier calls, conference callback delivery and the previously reported immediate busy/zero-duration rejection were not retested: this worktree has no configured live provider credentials or phone numbers. Busy alone does not prove a caller-ID cause. The configurable ID must already be owned/verified in Twilio; no numbers, billing or external account settings were changed. Voice-library recordings must be regenerated for the changed responses before production use. No production deployment was performed.

Twilio's official [Call resource](https://www.twilio.com/docs/voice/api/call-resource) confirms completion callbacks include busy/failed/no-answer/canceled and that outbound From must be owned or verified. The [Conference reference](https://www.twilio.com/docs/voice/twiml/conference) specifies start/end events, actual audio mixing at start, and callback configuration by the first participant; both caller/operator TwiML therefore carry the callback.

These external checks remain required before claiming production readiness.

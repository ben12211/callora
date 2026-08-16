# Changelog

Shared record of meaningful completed Callora changes.

## Unreleased

### Added

- Callora Control Plane: a server-rendered admin dashboard at `/dashboard`, served by the same Fastify process with no separate frontend to build or deploy. It covers sign-in, a dashboard home with platform counts, business management (create, edit, enable, disable, view detail), the full per-business agent configuration form, a provider status page, a calls list and call detail, settings with password rotation, and an audit history. Tenant text is HTML-escaped, dashboard responses are `no-store`, and every form post carries a per-session CSRF token.
- Admin authentication. Passwords are stored as scrypt hashes with a per-hash salt; sessions are server-side and only the SHA-256 of the cookie is persisted. `ADMIN_EMAIL` and `ADMIN_PASSWORD` create the bootstrap administrator at startup and reset its password when the value changes, so a fresh stack is always reachable; an unchanged value is detected and left alone, so a password rotated in the dashboard is not silently undone. Changing a password invalidates every session for that account. Login failures are indistinguishable between an unknown address and a wrong password, and the unknown-account path burns the same derivation cost.
- The management API is now behind authentication. `/api/*` accepts either a dashboard session cookie or the optional `ADMIN_API_KEY` machine credential as an `X-Api-Key` header. The Twilio webhooks are unchanged and still authenticate with Twilio's request signature.
- Per-business voice provider selection. `agent_configs.voice_provider` chooses `openai`, `elevenlabs`, or `cartesia` per tenant, and the media layer picks the bridge from the agent rather than from a single platform-wide setting. `VOICE_PROVIDER` now only supplies the default for newly created agents. The agent's `voice` and `model` are interpreted by the selected provider: an OpenAI voice name and realtime model, an ElevenLabs voice id sent as a per-call `tts.voice_id` override, or a Cartesia Sonic voice UUID with the reasoning model for the text turn.
- Provider status: `GET /api/providers` and the dashboard Providers page report which execution providers this deployment holds credentials for and which environment variables the others are missing. Credentials themselves are never returned or rendered.
- Agent configuration API: `GET`/`PUT /api/businesses/:id/agent`. Enabling an agent on a provider the platform cannot execute is rejected with `422`; saving it disabled is allowed, so a business can be prepared before its credentials land.
- Audit history. `audit_events` records business creation, updates, enable/disable, deletion, agent changes, sign-ins, and password changes, with the actor, a field-level diff, and a summary. Recording is best effort and never fails the write it describes. Readable at `GET /api/audit`, on the dashboard, and per business on its detail page.
- Migration `003_control_plane.sql`: `voice_provider` on `agent_configs`, and the `admin_users`, `admin_sessions`, and `audit_events` tables. Existing agents are backfilled onto the provider the deployment was actually running rather than onto the column default: the migration runner publishes the validated `VOICE_PROVIDER` and `TEXT_LLM_MODEL` as Postgres settings and the migration reads them, so upgrading an ElevenLabs or Cartesia deployment does not silently move its calls to OpenAI. For those two providers the stored `voice` is cleared, because the value was an OpenAI voice name and blank means "keep the voice already configured on the provider side"; a Cartesia deployment additionally has its realtime snapshot replaced with the reasoning model, which is what that pipeline's text turn needs.

### Changed

- Every business created through the control plane also gets an agent row, created disabled. The number keeps answering with the static greeting until the agent is configured and turned on, and the dashboard always has one shape to edit.
- An enabled agent whose provider has no platform credentials now answers with the business's static greeting and logs the mismatch, instead of opening a media stream that could not connect.
- The `agent_configs.voice` check now allows up to 80 characters and a blank value. `voice` is provider-scoped, and blank means "keep the voice the provider is already configured with"; OpenAI, which has no such default, still requires one.
- The deployment pipeline and both Compose files carry `ADMIN_EMAIL`, `ADMIN_PASSWORD`, `ADMIN_NAME`, `ADMIN_API_KEY`, and `SESSION_TTL_HOURS`. The secret channel to `deploy.sh` reads the three new values optionally, so an older workflow still deploys.

### Added

- Cartesia as a third voice provider, selected by `VOICE_PROVIDER=cartesia`. Unlike the speech-to-speech providers it is a pipeline Callora drives itself: Twilio → Cartesia streaming STT (`ink-whisper`) → a text LLM → Cartesia Sonic streaming TTS (`sonic-3.5-2026-05-04`) → Twilio. Both Cartesia sockets negotiate `pcm_mulaw` at 8000 Hz, so Twilio audio is never transcoded; the only conversion is base64, because Twilio wraps mu-law in JSON while Cartesia STT takes raw binary frames. LLM output is streamed into Sonic at clause boundaries on a shared `context_id` rather than buffered, so speech begins before the reply is finished. Barge-in cancels the context, clears Twilio's buffer, aborts the in-flight LLM turn, and — because Cartesia's `cancel` only halts generations that have not started — drops any audio still arriving for the abandoned context. `end_call` is an LLM tool and waits for both Sonic's `done` and Twilio's playback acknowledgement, so the goodbye is never cut off. The tenant locale is normalised to a bare code (`he-IL` → `he`) and the tenant prompt and greeting drive the pipeline unchanged. Credentials come from the `CARTESIA_API_KEY` secret and `CARTESIA_VOICE_ID` variable, synced through the existing deployment; the reasoning turn reuses `OPENAI_API_KEY` with a configurable `TEXT_LLM_MODEL`.

- ElevenLabs Agents as a second realtime voice provider, selected by `VOICE_PROVIDER` (`openai`, the default, or `elevenlabs`). Only the selected provider's credentials are required — `OPENAI_API_KEY`, or `ELEVENLABS_API_KEY` plus `ELEVENLABS_AGENT_ID` — and an unrecognised provider is rejected at startup and in the deployment pipeline. The existing OpenAI bridge is untouched; the media layer picks a bridge and the rest of the call path (Twilio routing by `To`, allowlist, stream token, call persistence, max call duration) is shared. Both providers run `ulaw_8000` end to end, so neither transcodes; an ElevenLabs agent negotiating another format ends the call rather than playing noise. Barge-in uses each provider's own mechanism, transcripts are logged from ElevenLabs' `user_transcript`/`agent_response` events, and `end_call` arrives as an ElevenLabs client tool that drives the same drain-then-hang-up sequence. The tenant `instructions`, `greeting`, and `language` are sent as per-call conversation overrides, so one policy covers both providers. The API key is used only to fetch a short-lived signed socket URL and never reaches Twilio or a log line.

- Caller allowlist. Numbers come from the `ALLOW_LIST` environment variable — stored as a GitHub repository secret and synced into the server environment on every deployment — or, when it is unset, from a gitignored `allowlist.local.js` at the project root documented by `allowlist.example.js`. Callers not on the list are answered with a hangup before any business lookup or stream token, so no Realtime session is opened. An empty variable, empty list, missing file, or malformed file allows every caller. `From` is used for this check only; tenant routing still uses `To` exclusively.
- Global policy rules against guessing at unclear caller speech: the agent may not invent a login, order, product, payment, account, or appointment context the caller never stated, must ask one short clarification question instead, and must treat an unclear possible goodbye as a closing rather than a new support issue.
- `near_field` input noise reduction for handset audio on the realtime session. The pcmu bridge is unchanged and no transcoding is introduced.

### Changed

- Agent brevity policy: the default turn is now one short sentence of roughly eight to twelve words, capped at three short sentences when detail is genuinely needed. The agent may not echo the caller's question back, may not open with filler, and must keep a clarification question to two or three words. Hebrew agents additionally get spoken-register notes naming `מה אמרת?` as the short form to use and the stiff customer-service phrasing to avoid; other languages are unaffected.
- Barge-in now stops the agent mid-word. On `speech_started` the bridge cancels the in-flight OpenAI response before truncating the item and clearing Twilio's queued audio, and it no longer waits for pending Twilio marks — a response that has not yet produced audio is cancelled too. Server VAD and `interrupt_response` are unchanged, and conversation context after an interruption is preserved by the existing truncate.

- Debug transcription now defaults to `gpt-4o-transcribe` (still configurable via `OPENAI_TRANSCRIBE_MODEL`) and sends a short customer-service prompt alongside the language hint, in Hebrew for Hebrew agents.

### Added

- Realtime conversation debug logging: caller audio transcription is enabled per session (`OPENAI_TRANSCRIBE_MODEL`, default `gpt-4o-mini-transcribe`, language hint derived from the agent locale), and each completed turn logs one `[conversation] USER: …` / `[conversation] AI: …` line tagged with `callId`, `businessId`, `callSid`, and `streamSid`. Audio payloads and credentials are never logged; transcripts are not persisted.
- The fully composed agent instructions are logged once per call at `debug` level only, for verifying the policy the model received.

### Added

- Global Callora agent policy applied to every realtime session: business-only scope with one-sentence refusal and redirect, prompt-injection resistance, and short phone turns with one question at a time. Per-business `instructions` are embedded as a delimited, lower-precedence block that cannot widen scope or disable a global rule.
- Internal `end_call` realtime tool. The model supplies only a reason; the server hangs up the `CallSid` the media stream was authorized for, after waiting for the goodbye audio to be acknowledged by Twilio. Termination is idempotent and falls back to closing the media stream if the Twilio REST call fails.
- Silence handling on top of server VAD: one "are you still there?" check after a long silence, then a goodbye and hangup after a second one, reset whenever the caller speaks.
- `src/telephony/call-terminator.ts`, an idempotent Twilio call terminator that treats already-completed calls as success.

### Changed

- `TWILIO_ACCOUNT_SID` is now required at startup and must be a well-formed Account SID; it authenticates the REST hangup. Every deployment path already required it.
- The demo seed agent now carries only business-specific context; phone style and scope rules come from the global policy.

### Added

- Bidirectional Twilio Media Streams (`<Connect><Stream>`) to OpenAI Realtime (`gpt-realtime-2.1`) speech-to-speech calls, bridging G.711 mu-law audio in both directions without transcoding, with server-VAD barge-in and an AI-spoken greeting.
- Token-authenticated `/webhooks/twilio/media` WebSocket endpoint, call-scoped and business-scoped, with `CallSid` verification on the stream `start` event.
- Per-business `agent_configs` (instructions, greeting, language, voice, realtime model, enabled) plus a Hebrew customer-service agent for the demo business.
- `twilio_stream_sid` and `openai_session_id` on call records, structured realtime lifecycle logs, and `OPENAI_API_KEY` configuration across `.env`, Compose, and the deployment secret sync.

### Changed

- The voice webhook now answers with `<Connect><Stream>` when the resolved business has an enabled agent; businesses without one keep the static `<Say>` greeting.
- Production CI/CD now publishes private ARM64 images to Docker Hub and deploys immutable commit-SHA releases with the existing health-gated rollback flow.

### Added

- Minimal shared instructions for Codex, Claude Code, and GitHub Copilot.
- Canonical task-specific Agent Skills under `.agents/skills/` with upstream licenses.
- Concise, updateable architecture and project state in `docs/AI_STATE.md`.

## 0.1.0 - 2026-08-13

### Added

- Multi-tenant Twilio/PostgreSQL backend foundation with signed webhooks, tenant greetings, business CRUD, call reads, tests, and health checks.
- ARM64 Docker/Caddy production deployment, GHCR CI/CD, Oracle Linux bootstrap, health-gated releases, and safe application rollback.

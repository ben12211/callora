# Callora AI State

Last updated: 2026-08-17

## Current product state

Callora is a multi-tenant phone customer-service backend. One service handles many Twilio numbers; inbound `To` selects the business and `From` identifies the caller. It stores businesses/calls, validates Twilio signatures, records status callbacks, and exposes business CRUD, call reads, and `/health`.

Inbound calls now run as real speech-to-speech AI conversations: the voice webhook resolves the business, loads its `agent_configs` row, and returns `<Connect><Stream>` TwiML pointing at `/webhooks/twilio/media`. Each Twilio Media Stream is paired with exactly one realtime session, bridging mu-law 8 kHz in both directions without transcoding, with barge-in, an AI-spoken greeting, and persisted `StreamSid`/session identifiers. Businesses without an enabled agent fall back to the static `<Say>` greeting.

Callora now ships a control plane: a server-rendered admin dashboard at `/dashboard`, served by the same Fastify process. It covers sign-in, a dashboard home, business management (create, edit, enable, disable, detail), the full per-business agent configuration form, a provider status page, a calls list and call detail, settings with password rotation, and an audit history. `/api` and `/dashboard` both require authentication: a server-side session cookie (only its SHA-256 is stored, every form post carries a per-session CSRF token) or the optional `ADMIN_API_KEY` machine credential. The Twilio webhooks stay outside that and authenticate with Twilio's signature. `ADMIN_EMAIL`/`ADMIN_PASSWORD` create the bootstrap administrator at startup and reset its password when the value changes. Important administrative changes are recorded in `audit_events` with the actor and a field-level diff.

Provider selection is per business, not per deployment. `agent_configs.voice_provider` chooses the execution provider for each tenant and the media layer picks the bridge from the agent row; `VOICE_PROVIDER` is now only the default for newly created agents. The agent's `voice` and `model` are interpreted by whichever provider is selected — an OpenAI voice name and realtime model, an ElevenLabs voice id sent as a per-call `tts.voice_id` override, or a Cartesia Sonic voice UUID with the reasoning model for the text turn. Platform provider credentials are managed from the dashboard's Providers page, stored in `platform_settings` keyed by the environment-variable name each one overrides. The environment is the floor: a saved value wins, clearing a field falls back to it, and settings are resolved in-process so a credential entered in the browser reaches the next call without a restart. Secrets are sealed with AES-256-GCM under `SECRETS_KEY` and are never rendered back to a browser; without that key the credential fields are disabled and only non-secret settings are editable. Missing credentials no longer stop startup — an agent still cannot be enabled on a provider the platform cannot execute, and a provider whose credentials are absent falls back to the static greeting instead of failing.

The provider backends themselves are unchanged: `openai` (the default, OpenAI Realtime `gpt-realtime-2.1`, server-VAD turn detection, barge-in via `response.cancel` + truncate + Twilio `clear`) or `elevenlabs` (ElevenLabs Agents over a signed WebSocket, barge-in via the provider's `interruption` event, `end_call` as a client tool). Only the selected provider's credentials are required. The two bridges are separate classes behind one media layer; Twilio routing, the allowlist, the stream token, call persistence, and the call-duration ceiling are shared. Tenant `instructions`/`greeting`/`language` reach ElevenLabs as per-call conversation overrides, so one policy covers both.

Agent behaviour is governed by a global Callora policy that outranks per-business `instructions`: the agent stays strictly within its own business, refuses unrelated topics in one sentence and redirects, ignores prompt-injection attempts, and defaults to one short sentence of roughly eight to twelve words with a single question. Tenant instructions are embedded as a delimited, lower-precedence block. The agent ends calls itself through an internal `end_call` tool that carries only a reason — the server hangs up the `CallSid` the stream was authorized for, after the goodbye audio is acknowledged by Twilio, idempotently and with a media-stream-close fallback. Long silences escalate from one "are you still there?" check to a goodbye and hangup, resetting whenever the caller speaks.

The policy also forbids guessing at unclear speech: the agent never invents a login, order, product, payment, account, or appointment context the caller did not state, asks one short clarification question instead, and treats an unclear possible goodbye as a closing. Caller audio uses `near_field` noise reduction ahead of turn detection, with the pcmu bridge unchanged.

Live calls are observable in the logs: caller audio transcription is enabled for logging only, and each completed turn emits one `[conversation] USER:` / `[conversation] AI:` line tagged with `callId`, `businessId`, `callSid`, and `streamSid`. Transcripts are logged, never persisted. At `debug` level the composed agent instructions are logged once per call.

MCP, Google Calendar, CRM connectors, WhatsApp, billing, knowledge/RAG, business-facing customer accounts, the full tool system, transcript persistence, and an agent builder/versioning are intentionally not implemented. Administration is authenticated but flat: every administrator is a platform administrator, with no per-tenant authorization yet.

## Stack and architecture

- Node.js 20+ / TypeScript, Fastify (+ `@fastify/websocket`), Zod, Twilio SDK, `ws`, `pg`
- PostgreSQL 16 with SQL migrations and an idempotent example seed
- Single backend with a PostgreSQL data store; no microservices or queues
- Vitest API tests, ESLint, strict TypeScript build
- Control plane in `src/auth/` (scrypt passwords, sessions, bootstrap administrator), `src/http/api-routes.ts`, and `src/http/dashboard/` (layout, pages, routes); no client-side framework or build step
- Dashboard-managed platform settings in `src/platform/` (`settings.ts` merges stored values over the environment and re-derives provider credentials; `secret-box.ts` seals them under `SECRETS_KEY`)
- Realtime call path in `src/realtime/` (global policy, provider list, per-provider protocol builders, one bridge and one connection module per provider), `src/http/media-stream.ts`, and `src/telephony/call-terminator.ts`
- `TWILIO_ACCOUNT_SID` is required at startup: the REST client that hangs calls up is authenticated per account
- Media Stream handshakes are authorized by a short-lived HMAC token minted by the signature-validated voice webhook
- Future integration boundaries live in `src/future/interfaces.ts`

## Deployment

- Multi-stage non-root Alpine backend image for `linux/arm64`
- Production Compose: backend + PostgreSQL + Caddy HTTPS proxy
- Persistent named PostgreSQL/Caddy volumes and container health checks
- GitHub Actions validates PRs; successful `main` pushes publish SHA/`latest` images to a private Docker Hub repository and deploy the immutable SHA over SSH
- Production target: Oracle Linux 9 ARM64; migrations are advisory-locked and failed releases restore the prior app configuration/image without deleting data

## Next major milestones

1. Add per-tenant authorization on top of the existing authentication, and business-facing accounts.
2. Persist call transcripts (currently logs only) and add reconnect/degradation handling for the realtime path.
3. Add one narrow mocked tool-call workflow on top of the existing agent configuration.
4. Integrate real business tools later; keep WhatsApp and voice cloning as separate future milestones.

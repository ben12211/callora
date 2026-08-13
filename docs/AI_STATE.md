# Callora AI State

Last updated: 2026-08-14

## Current product state

Callora is a multi-tenant phone customer-service backend. One service handles many Twilio numbers; inbound `To` selects the business and `From` identifies the caller. It stores businesses/calls, validates Twilio signatures, records status callbacks, and exposes business CRUD, call reads, and `/health`.

Inbound calls now run as real speech-to-speech AI conversations: the voice webhook resolves the business, loads its `agent_configs` row, and returns `<Connect><Stream>` TwiML pointing at `/webhooks/twilio/media`. Each Twilio Media Stream is paired with exactly one OpenAI Realtime session (`gpt-realtime-2.1`), bridging `audio/pcmu` in both directions without transcoding, with server-VAD turn detection, barge-in (truncate + Twilio `clear`), an AI-spoken greeting, and persisted `StreamSid`/session identifiers. Businesses without an enabled agent fall back to the static `<Say>` greeting.

Agent behaviour is governed by a global Callora policy that outranks per-business `instructions`: the agent stays strictly within its own business, refuses unrelated topics in one sentence and redirects, ignores prompt-injection attempts, and keeps turns to one to three sentences with a single question. Tenant instructions are embedded as a delimited, lower-precedence block. The agent ends calls itself through an internal `end_call` tool that carries only a reason — the server hangs up the `CallSid` the stream was authorized for, after the goodbye audio is acknowledged by Twilio, idempotently and with a media-stream-close fallback. Long silences escalate from one "are you still there?" check to a goodbye and hangup, resetting whenever the caller speaks.

The policy also forbids guessing at unclear speech: the agent never invents a login, order, product, payment, account, or appointment context the caller did not state, asks one short clarification question instead, and treats an unclear possible goodbye as a closing. Caller audio uses `near_field` noise reduction ahead of turn detection, with the pcmu bridge unchanged.

Live calls are observable in the logs: caller audio transcription is enabled for logging only, and each completed turn emits one `[conversation] USER:` / `[conversation] AI:` line tagged with `callId`, `businessId`, `callSid`, and `streamSid`. Transcripts are logged, never persisted. At `debug` level the composed agent instructions are logged once per call.

Business tool execution, CRM/order/appointment integrations, WhatsApp, voice cloning, transcripts, an agent builder/versioning, and authenticated administration are intentionally not implemented.

## Stack and architecture

- Node.js 20+ / TypeScript, Fastify (+ `@fastify/websocket`), Zod, Twilio SDK, `ws`, `pg`
- PostgreSQL 16 with SQL migrations and an idempotent example seed
- Single backend with a PostgreSQL data store; no microservices or queues
- Vitest API tests, ESLint, strict TypeScript build
- Realtime call path in `src/realtime/` (global policy, protocol builders, bridge, OpenAI connection), `src/http/media-stream.ts`, and `src/telephony/call-terminator.ts`
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

1. Add authenticated, tenant-scoped administration.
2. Persist call transcripts (currently logs only) and add reconnect/degradation handling for the realtime path.
3. Add one narrow mocked tool-call workflow on top of the existing agent configuration.
4. Integrate real business tools later; keep WhatsApp and voice cloning as separate future milestones.

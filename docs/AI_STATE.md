# Callora AI State

Last updated: 2026-08-14

## Current product state

Callora is a multi-tenant phone customer-service backend. One service handles many Twilio numbers; inbound `To` selects the business and `From` identifies the caller. It stores businesses/calls, validates Twilio signatures, records status callbacks, and exposes business CRUD, call reads, and `/health`.

Inbound calls now run as real speech-to-speech AI conversations: the voice webhook resolves the business, loads its `agent_configs` row, and returns `<Connect><Stream>` TwiML pointing at `/webhooks/twilio/media`. Each Twilio Media Stream is paired with exactly one OpenAI Realtime session (`gpt-realtime-2.1`), bridging `audio/pcmu` in both directions without transcoding, with server-VAD turn detection, barge-in (truncate + Twilio `clear`), an AI-spoken greeting, and persisted `StreamSid`/session identifiers. Businesses without an enabled agent fall back to the static `<Say>` greeting.

Tool execution, CRM/order/appointment integrations, WhatsApp, voice cloning, transcripts, an agent builder/versioning, and authenticated administration are intentionally not implemented.

## Stack and architecture

- Node.js 20+ / TypeScript, Fastify (+ `@fastify/websocket`), Zod, Twilio SDK, `ws`, `pg`
- PostgreSQL 16 with SQL migrations and an idempotent example seed
- Single backend with a PostgreSQL data store; no microservices or queues
- Vitest API tests, ESLint, strict TypeScript build
- Realtime call path in `src/realtime/` (protocol builders, bridge, OpenAI connection) and `src/http/media-stream.ts`
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
2. Persist call transcripts and add reconnect/degradation handling for the realtime path.
3. Add one narrow mocked tool-call workflow on top of the existing agent configuration.
4. Integrate real business tools later; keep WhatsApp and voice cloning as separate future milestones.

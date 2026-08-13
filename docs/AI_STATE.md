# Callora AI State

Last updated: 2026-08-13

## Current product state

Callora is a multi-tenant phone customer-service backend. One service handles many Twilio numbers; inbound `To` selects the business and `From` identifies the caller. Push 1 stores businesses/calls, returns tenant-specific TwiML greetings, validates Twilio signatures, records status callbacks, and exposes business CRUD, call reads, and `/health`.

AI conversations, WebSockets, Twilio Media Streams, tool execution, CRM/order/appointment integrations, WhatsApp, and voice cloning are intentionally not implemented.

## Stack and architecture

- Node.js 20+ / TypeScript, Fastify, Zod, Twilio SDK, `pg`
- PostgreSQL 16 with SQL migrations and an idempotent example seed
- Single backend with a PostgreSQL data store; no microservices or queues
- Vitest API tests, ESLint, strict TypeScript build
- Future integration boundaries live in `src/future/interfaces.ts`

## Deployment

- Multi-stage non-root Alpine backend image for `linux/arm64`
- Production Compose: backend + PostgreSQL + Caddy HTTPS proxy
- Persistent named PostgreSQL/Caddy volumes and container health checks
- GitHub Actions validates PRs; successful `main` pushes publish SHA/`latest` images to a private Docker Hub repository and deploy the immutable SHA over SSH
- Production target: Oracle Linux 9 ARM64; migrations are advisory-locked and failed releases restore the prior app configuration/image without deleting data

## Next major milestones

1. Add authenticated, tenant-scoped administration.
2. Add the first Twilio Media Streams + OpenAI Realtime call path with lifecycle, reconnect, and observability controls.
3. Add per-business prompt/voice configuration and one narrow mocked tool-call workflow.
4. Integrate real business tools later; keep WhatsApp and voice cloning as separate future milestones.

# Callora

Callora is the Push 1 backend foundation for a multi-tenant phone customer-service platform. It is one Node.js/TypeScript service backed by PostgreSQL. Every Twilio number can use the same webhook; the service resolves the business from Twilio's incoming `To` number, records the call, and returns that business's greeting as TwiML.

This push deliberately does **not** include AI, WebSockets, Twilio Media Streams, WhatsApp, voice cloning, or external CRM integrations.

## What is included

- Fastify + TypeScript HTTP service
- PostgreSQL schema, migration runner, and idempotent example seed
- Multi-tenant business lookup by E.164 `To` number only
- Signed Twilio voice webhook returning TwiML
- Signed Twilio call-status callback
- Business CRUD and read-only call APIs
- Database-aware health endpoint
- Docker and Docker Compose setup using ARM64-compatible official images
- Unit/API tests, ESLint, and strict TypeScript compilation
- Small, implementation-free interfaces for future realtime AI, Media Streams, tool calling, business systems, WhatsApp, and voice providers

## Prerequisites

- Docker with Docker Compose (recommended), or Node.js 20+ with pnpm and PostgreSQL
- A Twilio account and phone number when testing real calls
- A public HTTPS URL that forwards to the backend (for example a development tunnel)

No paid resources are created by this repository.

## Run with Docker Compose

1. Copy `.env.example` to `.env`.
2. Set `TWILIO_AUTH_TOKEN` to the Auth Token from the Twilio Console.
3. Set `PUBLIC_BASE_URL` to the exact public origin Twilio will call, with no trailing slash. Example: `https://abc123.example-tunnel.app`.
4. Set `OPENAI_API_KEY` to an OpenAI API key with Realtime access; it is used server-side for the speech-to-speech call bridge.
5. Start the stack:

   ```bash
   docker compose up --build
   ```

The API is available at `http://localhost:3000`. The container runs migrations and the idempotent seed before starting. PostgreSQL data is retained in the `postgres_data` volume.

Check health:

```bash
curl http://localhost:3000/health
```

## Run locally without Docker

Start a PostgreSQL database, copy `.env.example` to `.env`, and make sure `DATABASE_URL` points to it. Then run:

```bash
pnpm install
pnpm db:migrate
pnpm db:seed
pnpm dev
```

The example seed creates:

- Business: `Callora Demo Business`
- Twilio number: `+15551234567` (replace it through the API with a number you own)
- ID: `00000000-0000-4000-8000-000000000001`

Useful checks:

```bash
pnpm lint
pnpm build
pnpm test
```

For the ARM64 Oracle Linux production topology, GitHub Actions deployment, required secrets, and rollback behavior, see [DEPLOYMENT.md](./DEPLOYMENT.md).

## Configure Twilio

For each Twilio phone number, configure the same endpoints using HTTP `POST`:

- **A call comes in:** `https://your-public-host.example.com/webhooks/twilio/voice`
- **Call status callback:** `https://your-public-host.example.com/webhooks/twilio/call-status`

Add one business row per Twilio number. Phone numbers must be E.164 (for example `+14155552671`). The incoming voice route always selects the tenant using Twilio's `To` field. `From` is stored only when it is a valid E.164 number and is never used to select a business.

### Realtime speech-to-speech calls

When the resolved business has an enabled row in `agent_configs`, the voice webhook answers with `<Connect><Stream>` and Twilio opens a bidirectional Media Stream to `wss://<PUBLIC_BASE_URL host>/webhooks/twilio/media`. Callora then opens one OpenAI Realtime session per stream (`gpt-realtime-2.1` by default) and bridges G.711 mu-law audio in both directions without transcoding, with server VAD barge-in. Businesses without an enabled agent keep the previous static `<Say>` greeting.

Twilio does not sign the WebSocket handshake, so the voice webhook issues a short-lived HMAC token bound to the `CallSid` and business, and the media endpoint rejects any handshake without a valid token. The `start` event's `CallSid` must also match the token.

Twilio signatures are validated against `TWILIO_AUTH_TOKEN` and the exact URL assembled from `PUBLIC_BASE_URL` plus the request path. A mismatch returns `403`, so the configured public origin and Twilio webhook URL must match exactly, including HTTPS and any path prefix.

## Endpoints

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/health` | Service and database health |
| `GET` | `/api/businesses` | List businesses |
| `POST` | `/api/businesses` | Create a business |
| `GET` | `/api/businesses/:id` | Read a business |
| `PATCH` | `/api/businesses/:id` | Update a business |
| `DELETE` | `/api/businesses/:id` | Delete a business if it has no call history |
| `GET` | `/api/calls?businessId=&limit=&offset=` | List calls (maximum page size 100) |
| `GET` | `/api/calls/:id` | Read a call |
| `POST` | `/webhooks/twilio/voice` | Signed incoming-call webhook; returns TwiML |
| `POST` | `/webhooks/twilio/call-status` | Signed call lifecycle callback |

Example business creation:

```bash
curl -X POST http://localhost:3000/api/businesses \
  -H 'content-type: application/json' \
  -d '{
    "name": "Acme Dental",
    "phoneNumber": "+14155552671",
    "greeting": "Thanks for calling Acme Dental. How can we help?",
    "active": true
  }'
```

Deleting a business with call history returns `409`; deactivate it with `PATCH` instead so historical calls remain intact.

## Project layout

```text
src/
  app.ts                  application factory and error handling
  server.ts               process startup and graceful shutdown
  config.ts               validated environment configuration
  db/                     PostgreSQL pool, store, migrations, seed
  domain/                 persisted domain types
  http/                   API routes, validation, Twilio signature guard
  future/interfaces.ts    intentionally unimplemented Push 2+ seams
migrations/               ordered SQL migrations
test/                     API-level tests using an in-memory store
```

## Production notes

- Put the backend behind HTTPS and an ingress/reverse proxy suitable for your host.
- Change the Compose database password outside local development.
- The CRUD API is intentionally unauthenticated in Push 1; do not expose `/api` publicly before adding authentication and authorization.
- Keep `TWILIO_AUTH_TOKEN`, `OPENAI_API_KEY`, `DATABASE_URL`, and all future provider credentials in environment variables or a deployment secret manager.
- Back up PostgreSQL and monitor webhook errors before onboarding real businesses.

## Recommended Push 2

Push 2 should add authenticated admin access with tenant-scoped authorization, then implement the first end-to-end voice conversation path using Twilio Media Streams and OpenAI Realtime. It should include a call state machine, WebSocket lifecycle/reconnect handling, per-business prompt and voice configuration, usage/error telemetry, and integration tests. Tool calling should start with one narrow, mocked business tool before connecting any real CRM, order, or appointment system. WhatsApp and voice cloning should remain separate later pushes.

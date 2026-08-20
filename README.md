# Callora

Callora is the Push 1 backend foundation for a multi-tenant phone customer-service platform. It is one Node.js/TypeScript service backed by PostgreSQL. Every Twilio number can use the same webhook; the service resolves the business from Twilio's incoming `To` number, records the call, and returns that business's greeting as TwiML.

This push deliberately does **not** include AI, WebSockets, Twilio Media Streams, WhatsApp, voice cloning, or external CRM integrations.

## What is included

- Server-rendered admin control plane at `/dashboard` behind a password login
- Fastify + TypeScript HTTP service
- PostgreSQL schema, migration runner, and idempotent example seed
- Multi-tenant business lookup by E.164 `To` number only
- Signed Twilio voice webhook returning TwiML
- Signed Twilio call-status callback
- Business CRUD, per-business agent configuration, and read-only call APIs, all behind authentication
- Per-tenant authorization: an administrator is either a platform administrator or scoped to exactly one business
- Persisted conversation transcripts with a retention window
- Per-business voice provider selection across OpenAI, ElevenLabs, and Cartesia
- Provider status reporting and an audit trail of administrative changes
- Prometheus metrics at `/metrics` for the call path
- Rate limiting on sign-in and the management API
- Health endpoint reporting the database, live calls, and provider readiness
- Graceful shutdown that drains live calls instead of cutting them off
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
5. Set `ADMIN_EMAIL` and `ADMIN_PASSWORD`; they create the dashboard administrator. The Compose defaults (`admin@callora.local` / `callora-dev-password`) are for local development only.
6. Start the stack:

   ```bash
   docker compose up --build
   ```

The API is available at `http://localhost:3000` and the dashboard at
`http://localhost:3000/dashboard`. The container runs migrations and the idempotent seed
before starting, and creates the administrator from the environment. PostgreSQL data is
retained in the `postgres_data` volume.

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

Then open `http://localhost:3000/dashboard` and sign in with `ADMIN_EMAIL` and `ADMIN_PASSWORD`.

The example seed creates:

- Administrator: `ADMIN_EMAIL`, when it and `ADMIN_PASSWORD` are set
- Business: `Callora Demo Business`
- Twilio number: `+15551234567` (replace it through the API with a number you own)
- ID: `00000000-0000-4000-8000-000000000001`

Useful checks:

```bash
pnpm lint
```

```bash
pnpm typecheck
```

```bash
pnpm build
```

```bash
pnpm test
```

## Administrator roles

An administrator is either a **platform** administrator, who reaches every business and
the platform's provider credentials, or a **business** administrator, scoped to exactly
one tenant. Accounts default to `platform`, so an existing deployment is unchanged.

A business administrator sees only their own business, its calls and transcripts, and the
audit entries about it. Creating or deleting a business, the Providers page, and
`/metrics` stay platform-only. Asking about another tenant is answered `404` rather than
`403`, so the platform's customer list cannot be probed.

`ADMIN_API_KEY` is a platform credential and is not scoped.

## Transcripts

Each completed turn is stored against its call, readable on the call detail page and at
`GET /api/calls/:id/transcript`. They are the caller's own words, so they are deleted
after `TRANSCRIPT_RETENTION_DAYS` (default 30). Setting it to `0` keeps them indefinitely.

## Metrics

`GET /metrics` renders the Prometheus text format behind the same credential as the rest
of `/api`: active calls, first-audio latency, barge-ins, call outcomes and durations, and
how often a call fell back to the static greeting.

## Rotating secrets

`STREAM_TOKEN_SECRET` signs the short-lived Media Stream handshake token. Leave it unset
and the Twilio auth token signs it, as before. Both are accepted while you introduce it,
so no call in flight is dropped.

`SECRETS_KEY` encrypts the provider credentials stored from the dashboard. To change it
without losing them:

```bash
SECRETS_KEY_OLD=current SECRETS_KEY_NEW=replacement pnpm secrets:rotate --dry-run
```

Then run it without `--dry-run`, set `SECRETS_KEY` to the new value, and restart.

For the ARM64 Oracle Linux production topology, GitHub Actions deployment, required secrets, and rollback behavior, see [DEPLOYMENT.md](./DEPLOYMENT.md).

## Configure Twilio

For each Twilio phone number, configure the same endpoints using HTTP `POST`:

- **A call comes in:** `https://your-public-host.example.com/webhooks/twilio/voice`
- **Call status callback:** `https://your-public-host.example.com/webhooks/twilio/call-status`

Add one business row per Twilio number. Phone numbers must be E.164 (for example `+14155552671`). The incoming voice route always selects the tenant using Twilio's `To` field. `From` is stored only when it is a valid E.164 number and is never used to select a business.

### Realtime speech-to-speech calls

When the resolved business has an enabled row in `agent_configs`, the voice webhook answers with `<Connect><Stream>` and Twilio opens a bidirectional Media Stream to `wss://<PUBLIC_BASE_URL host>/webhooks/twilio/media`. Callora then opens one realtime session per stream and bridges G.711 mu-law audio in both directions without transcoding, with barge-in. Businesses without an enabled agent keep the previous static `<Say>` greeting.

Twilio does not sign the WebSocket handshake, so the voice webhook issues a short-lived HMAC token bound to the `CallSid` and business, and the media endpoint rejects any handshake without a valid token. The `start` event's `CallSid` must also match the token.

#### Voice providers

`VOICE_PROVIDER` selects the realtime backend for the whole deployment. Twilio routing, the caller allowlist, call persistence, and the stream token are identical either way; only the session behind the media stream changes.

| `VOICE_PROVIDER` | Backend | Required credentials |
| --- | --- | --- |
| `openai` (default) | OpenAI Realtime, `gpt-realtime-2.1` by default | `OPENAI_API_KEY` |
| `elevenlabs` | ElevenLabs Agents | `ELEVENLABS_API_KEY`, `ELEVENLABS_AGENT_ID` |
| `cartesia` | Cartesia STT -> text LLM -> Cartesia Sonic TTS | `CARTESIA_API_KEY`, `CARTESIA_VOICE_ID`, `OPENAI_API_KEY` |

Credentials can be entered on the dashboard's **Providers** page instead of in the environment, so a deployment can come up with none of them and be configured in the browser. A missing credential is reported at startup and on that page rather than stopping the process; until it is supplied, calls answer with the business's static greeting. An unrecognised provider is still rejected outright.

All three carry G.711 mu-law at 8 kHz — the format Twilio Media Streams already speak — so no path transcodes. The ElevenLabs session reports the formats it actually chose in `conversation_initiation_metadata`; if the agent is configured for anything other than mu-law, Callora ends the call with an error rather than playing noise to the caller.

Barge-in works on all three, through each provider's own mechanism: on OpenAI, server VAD plus an explicit `response.cancel` and buffer clear; on ElevenLabs, the `interruption` event, which drops whatever Twilio still has queued. The `end_call` behaviour is the same on both — the agent asks to hang up, Callora waits for the goodbye audio to be acknowledged by Twilio, then terminates the `CallSid` the stream was authorized for. On ElevenLabs this arrives as a **client tool** named `end_call`, which has to be registered on the agent (see below).

Running ElevenLabs requires configuring the agent once in the ElevenLabs dashboard:

- Set both the agent output and user input audio formats to `ulaw_8000`.
- Under the agent's **Security** tab, enable the overrides Callora sends per call: **system prompt**, **first message**, and **language**. Overrides are disabled by default, and sending one that is not enabled fails the conversation rather than being ignored.
- Add a **client tool** named `end_call` with a single optional string parameter `reason`, so the agent can hang up.

The tenant `instructions`, `greeting`, and `language` from `agent_configs` are sent as per-call overrides, so the same policy and greeting apply on every provider.

##### Cartesia

Cartesia is the only provider where Callora owns the turn loop: it is assembled from Cartesia streaming STT, a text LLM, and Cartesia Sonic streaming TTS rather than a single vendor speech-to-speech session.

- STT runs on `ink-whisper` (`CARTESIA_STT_MODEL`), the multilingual model. `ink-2` is faster but English-only, so it cannot serve a Hebrew tenant.
- TTS runs on `sonic-3.5-2026-05-04` (`CARTESIA_TTS_MODEL`). Hebrew needs the sonic-3 or sonic-3.5 family; sonic-2 and sonic-turbo do not carry it.
- Both sockets are opened with `pcm_mulaw` at 8000 Hz, so Twilio audio crosses untranscoded. The only conversion anywhere is base64, because Twilio wraps mu-law in JSON while Cartesia STT takes raw binary frames.
- The tenant locale is normalised to a bare code, so a `he-IL` business becomes `he`.
- LLM output is streamed into Sonic at clause boundaries on a shared `context_id`, so speech starts long before the reply is complete.
- Barge-in cancels the Sonic context, clears Twilio's buffer, and aborts the in-flight LLM turn. Cartesia documents `cancel` as halting only generations that have **not started**, so audio from a cancelled context keeps arriving; the bridge therefore also drops any chunk whose `context_id` is no longer active. Cancel alone does not stop the agent.
- `end_call` is an LLM tool. The hangup waits for Sonic to report `done` **and** for Twilio to acknowledge playback, so the goodbye is never cut off.
- The reasoning turn reuses `OPENAI_API_KEY` with a configurable text model (`TEXT_LLM_MODEL`, default `gpt-4o-mini`). It is deliberately not coupled to OpenAI Realtime audio.

Set `CARTESIA_VOICE_ID` to a Sonic voice UUID that suits the tenant language.

#### Agent behaviour

Every session is configured with a global Callora policy (`src/realtime/policy.ts`) that sits above the per-business `instructions`:

- The agent handles only its own business, refuses unrelated topics in one sentence and redirects, and never acts as a general assistant.
- Prompt-injection attempts ("ignore your instructions", "act as ChatGPT", claims of authority) are treated as unrelated topics. Caller speech is content, never instructions.
- Business `instructions` are embedded as a delimited, lower-precedence block: they can narrow behaviour but never widen scope or disable a global rule.
- Answers default to one short sentence of roughly eight to twelve words, never more than three, with a single question per turn. The agent does not echo the caller's question back or open with filler, and it moves toward closing once the request is handled.
- Unclear, garbled, or incomplete speech is never guessed at. The agent may not invent a login problem, order, product, payment, account issue, or any other context the caller did not state; it asks one short clarification question instead. Unclear speech that might be a goodbye is treated as a closing, never as a new support issue.

Caller audio uses `near_field` input noise reduction, which suits a handset on a narrowband phone line and cleans the signal ahead of turn detection. It does not alter the pcmu bridge and adds no transcoding.

The agent hangs up through an internal `end_call` tool. It takes only a `reason` — never a call identifier — and the server terminates the `CallSid` that the stream was authorized for. Callora waits for the goodbye audio to be acknowledged by Twilio before ending the call, and termination is idempotent: repeated tool calls, a caller who hangs up first, and a failed REST hangup all converge on one clean teardown.

#### Conversation logs

On ElevenLabs, transcripts come from the provider's own `user_transcript` and `agent_response` events and produce the same lines, with no separate transcription model to configure.

On OpenAI, caller audio is transcribed (`OPENAI_TRANSCRIBE_MODEL`, default `gpt-4o-transcribe`, with a language hint and a short customer-service prompt derived from the agent's locale) purely so live calls are observable. Each completed turn produces one line:

```text
[conversation] USER: שלום, רציתי לבדוק את הסטטוס של ההזמנה
[conversation] AI: בשמחה, מה מספר ההזמנה?
```

Every line carries `callId`, `businessId`, `callSid`, and `streamSid` as structured fields. Audio payloads, credentials, and raw events are never logged, and transcripts are collapsed to a single line and capped at 500 characters. Transcripts are not persisted to PostgreSQL.

At `LOG_LEVEL=debug` the bridge additionally logs the fully composed agent instructions once per call, so the policy the model actually received can be verified. This is deliberately excluded from the default `info` level.

Silence is handled with the server-VAD signal: after about 12 seconds without caller speech the agent asks once whether they are still there, and after another 12 seconds it says goodbye and ends the call. Any caller speech resets the escalation.

Twilio signatures are validated against `TWILIO_AUTH_TOKEN` and the exact URL assembled from `PUBLIC_BASE_URL` plus the request path. A mismatch returns `403`, so the configured public origin and Twilio webhook URL must match exactly, including HTTPS and any path prefix.

## Caller allowlist

While testing against a real Twilio number, you can restrict who reaches the agent. There are two sources, and the environment wins:

**`ALLOW_LIST` environment variable** — comma-separated E.164 numbers. This is what a deployed server uses; it is stored as the GitHub repository secret `ALLOW_LIST` and written into `/opt/callora/.env` on every deployment.

```bash
ALLOW_LIST=+972501234567,+972509998888
```

**`allowlist.local.js`** — used when `ALLOW_LIST` is unset or blank. Copy the example file and add your own numbers:

```bash
cp allowlist.example.js allowlist.local.js
```

```js
export const allow_list = [
  "+972501234567",
  "+972509998888"
];
```

`allowlist.local.js` is gitignored and excluded from the Docker build, so it never reaches production. On every incoming call the voice webhook compares Twilio's `From` against the list and, for a caller that is not on it, answers with a short message and `<Hangup/>` — before any business lookup, before a stream token is minted, and therefore before any OpenAI Realtime session is opened.

- Numbers are normalised to E.164, so spaces, dashes, and parentheses are fine; a number without a country code is rejected rather than guessed at.
- An empty variable, an empty list, a missing file, or a malformed file disables the allowlist and allows every caller. That is the default production behaviour.
- `From` is used for this check only. Business routing always resolves the tenant from `To`.
- Startup logs one line naming the source (`ALLOW_LIST` or `allowlist.local.js`) and whether the allowlist is active.

## Control plane

The dashboard is served by the same process at `http://localhost:3000/dashboard`. It is
plain server-rendered HTML: there is no separate frontend to build, install, or deploy.

Sign in with `ADMIN_EMAIL` and `ADMIN_PASSWORD` from your environment. Those two values
create the administrator on startup and reset its password whenever the value changes, so
a fresh stack always has a way in. Change the password from **Settings** afterwards, which
signs every session out; leave the environment values blank from then on, or the next
restart resets the password back.

The intended flow is:

```text
Login -> Create Business -> Configure Agent -> Choose Voice Provider/Voice -> Save -> Call the number
```

| Page | What it does |
| --- | --- |
| Home | Business, agent, and call counts, plus recent calls and admin changes |
| Businesses | List, create, edit, enable, and disable businesses |
| Business detail | Business fields, the agent configuration form, recent calls, and that business's change history |
| Calls | Every recorded call, filterable by business, with a detail page per call |
| Providers | Provider credentials, models, endpoints, the default provider, and the caller allowlist — all editable here |
| Audit history | Administrative changes, newest first |
| Settings | Your account, password rotation, platform configuration, and the administrator list |

A business is a Callora tenant. Each one owns exactly one agent configuration:

| Field | Meaning |
| --- | --- |
| Enabled | Off answers with the business's static greeting instead of the realtime agent |
| Language | BCP-47 tag such as `he-IL`; the agent always answers in it |
| Greeting | The first thing the agent says |
| Instructions | Business context only; the Callora phone-agent policy is applied on top and cannot be overridden |
| Voice provider | `openai`, `elevenlabs`, or `cartesia`, chosen per business |
| Voice | OpenAI voice name, ElevenLabs voice id, or Cartesia Sonic voice UUID. Blank keeps the provider's own configured voice; OpenAI requires one |
| Model | OpenAI realtime model, or the reasoning model for the Cartesia pipeline |

### Platform settings

Provider credentials belong to the platform, not to a business, and are managed on the
**Providers** page. Every field there is named after the environment variable it overrides:
a value saved in the dashboard wins, and clearing it falls back to the environment. Saved
values take effect on the next call, without a restart.

| Setting | Where |
| --- | --- |
| Default provider for new agents, caller allowlist | Platform |
| `OPENAI_API_KEY`, realtime endpoint, transcription model | OpenAI |
| `ELEVENLABS_API_KEY`, agent id, API base URL | ElevenLabs |
| `CARTESIA_API_KEY`, voice id, TTS/STT models, API version, reasoning model and endpoint | Cartesia |

API keys are encrypted with AES-256-GCM under `SECRETS_KEY` before they are stored, so a
database dump or backup carries nothing usable on its own. That key lives in the
environment and must stay the same for the life of the deployment: change it and every
stored credential becomes unreadable and has to be entered again. Without a `SECRETS_KEY`
the credential fields are disabled — everything else stays editable — rather than writing
keys to the database in the clear.

Changes reach the running server on their own. The instance that handled the save applies
it immediately; every other reader — a second instance, or a value written straight into the
database — re-checks the table when its snapshot ages out, which the call path does as it
answers, so a credential saved in the browser is in force within seconds without a restart
or a redeployment. If somebody else changed the settings while your page was open, saving is
refused with a message rather than rolling their change back: reload and make the change
again.

A stored credential is never rendered back to the browser, not even masked: the page shows
only that one exists and where it came from. Entering a new value replaces it; leaving the
field blank keeps it. An agent cannot be enabled on a provider this deployment holds no
credentials for, and if those credentials disappear later the call falls back to the static
greeting rather than failing.

## Authentication

Everything under `/api` and `/dashboard` requires authentication. Two credentials are
accepted:

- The dashboard session cookie, issued by the login form. Sessions are stored server-side,
  only their SHA-256 is persisted, and every form post carries a per-session CSRF token.
- `ADMIN_API_KEY`, sent as an `X-Api-Key` header. This is a machine credential for scripts
  and deployment checks; it cannot drive the dashboard forms. Leave it unset to disable it.

The Twilio webhooks are deliberately outside this: they authenticate with Twilio's own
request signature, exactly as before.

## Endpoints

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/health` | Service and database health (unauthenticated) |
| `GET` | `/dashboard/*` | Admin control plane (session cookie) |
| `GET` | `/api/businesses` | List businesses |
| `POST` | `/api/businesses` | Create a business, with a disabled agent row |
| `GET` | `/api/businesses/:id` | Read a business and its agent |
| `PATCH` | `/api/businesses/:id` | Update a business |
| `DELETE` | `/api/businesses/:id` | Delete a business if it has no call history |
| `GET` | `/api/businesses/:id/agent` | Read the agent configuration |
| `PUT` | `/api/businesses/:id/agent` | Replace the agent configuration |
| `GET` | `/api/providers` | Provider availability, without credentials |
| `GET` | `/api/calls?businessId=&limit=&offset=` | List calls (maximum page size 100) |
| `GET` | `/api/calls/:id` | Read a call |
| `GET` | `/api/audit?entityType=&entityId=&limit=&offset=` | Administrative history |
| `GET` | `/api/me` | The authenticated caller |
| `POST` | `/webhooks/twilio/voice` | Signed incoming-call webhook; returns TwiML |
| `POST` | `/webhooks/twilio/call-status` | Signed call lifecycle callback |

Example business creation:

```bash
curl -X POST http://localhost:3000/api/businesses \
  -H 'content-type: application/json' \
  -H "x-api-key: $ADMIN_API_KEY" \
  -d '{
    "name": "Acme Dental",
    "phoneNumber": "+14155552671",
    "greeting": "Thanks for calling Acme Dental. How can we help?",
    "active": true
  }'
```

Example agent configuration:

```bash
curl -X PUT "http://localhost:3000/api/businesses/$BUSINESS_ID/agent" \
  -H 'content-type: application/json' \
  -H "x-api-key: $ADMIN_API_KEY" \
  -d '{
    "enabled": true,
    "language": "he-IL",
    "greeting": "Hello, how can I help?",
    "instructions": "Answer questions about the clinic and book appointments.",
    "voiceProvider": "openai",
    "voice": "marin",
    "realtimeModel": "gpt-realtime-2.1"
  }'
```

Deleting a business with call history returns `409`; deactivate it with `PATCH` instead so historical calls remain intact.

## Project layout

```text
src/
  app.ts                  application factory and error handling
  server.ts               process startup and graceful shutdown
  config.ts               validated environment configuration
  auth/                   password hashing, sessions, bootstrap administrator
  db/                     PostgreSQL pool, store, migrations, seed
  domain/                 persisted domain types
  http/                   API routes, validation, Twilio signature guard
  http/dashboard/         server-rendered control plane pages
  realtime/               policy, protocol builders, and the Twilio<->OpenAI bridge
  telephony/              Twilio REST call termination
  future/interfaces.ts    intentionally unimplemented Push 2+ seams
migrations/               ordered SQL migrations
test/                     API-level tests using an in-memory store
```

## Production notes

- Put the backend behind HTTPS and an ingress/reverse proxy suitable for your host.
- Change the Compose database password outside local development.
- `/api` and `/dashboard` require authentication. Set a strong `ADMIN_PASSWORD`, and set `ADMIN_API_KEY` only if something other than a browser needs the management API.
- Serve the dashboard over HTTPS. The session cookie is marked `Secure` only when `PUBLIC_BASE_URL` is an `https://` origin.
- Keep `TWILIO_AUTH_TOKEN`, `OPENAI_API_KEY`, `DATABASE_URL`, and all future provider credentials in environment variables or a deployment secret manager.
- Back up PostgreSQL and monitor webhook errors before onboarding real businesses.

## Not in this push

These are deliberately absent and belong to later pushes: MCP, Google Calendar, CRM
connectors, WhatsApp, billing, knowledge/RAG, business-facing customer accounts, and the
full tool system. The admin model is also intentionally flat — every administrator is a
platform administrator, and there is no per-tenant authorization yet.

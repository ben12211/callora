# Callora V2

A generic AI phone dispatcher. A business is described in a validated **Business JSON**
file, and Callora answers its phone. It understands the caller, runs a structured business
flow with explicit state, performs real business actions, and answers mostly from
pre-generated audio.

```text
Incoming call → pre-generated greeting → streaming STT → meta + business intent
→ pipeline / slot filling (explicit state) → business action → response planner
→ cached audio | template audio | dynamic TTS → telephone audio
```

This is deliberately not a speech-to-speech model: telephony, STT, understanding, state,
actions, response planning, audio, barge-in and handoff are separate parts that are each
testable. The product specification is [`docs/callora_voice_agent_architecture.md`](docs/callora_voice_agent_architecture.md).
How V2 implements it is in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md). The first
complete business is a taxi company: [`businesses/taxi.json`](businesses/taxi.json).

The previous TypeScript implementation is preserved on the `OLD-MAIN` branch. What V2
kept from it is listed in [`docs/LEGACY_INVENTORY.md`](docs/LEGACY_INVENTORY.md).

## Run it (Docker is the only requirement)

```bash
./dev test                 # full test suite (Rust toolchain + Postgres in containers)
./dev check                # what CI runs: rustfmt, clippy -D warnings, tests
./dev simulate taxi        # talk to the taxi business in text, through the real engine
./dev callora understand 'צריך מונית לנתב"ג, אנחנו שלושה'
./dev up                   # Postgres + server on http://localhost:3000
./dev logs / ./dev down / ./dev clean
```

Nothing is installed on the host. The toolchain, the cargo cache, build output and the
database all live in containers and named volumes. Behind a TLS-inspecting proxy, add a
git-ignored `docker-compose.local.yml` that mounts your CA into the `toolchain` and `app`
services (`./dev` picks it up automatically).

To make real calls: copy `.env.example` to `.env`, fill in the Twilio, Cartesia,
ElevenLabs (and optionally OpenAI) settings, expose port 3000 over HTTPS (for example with
a tunnel), set `PUBLIC_BASE_URL` and `TWILIO_SKIP_SIGNATURE_VALIDATION=false`, and point
the Twilio number's voice webhook at `https://<host>/webhooks/twilio/voice`. Generate the
voice library once:

```bash
./dev callora voice-library build --business taxi --out target/voice-library
```

## Workspace

| Crate | What it is |
| --- | --- |
| `callora-core` | The generic runtime with no I/O: the Business JSON schema and validation, understanding (fast path + LLM request/parse), explicit call state, the engine (meta intents, pipelines, slots, rules, confirmation, fallback, handoff), response planning, Hebrew numbers and spoken-text normalization |
| `callora-audio` | μ-law, VAD (barge-in + endpointing), the pre-generated voice library and its builder, the TTS cache, and paced, cancellable playout |
| `callora-runtime` | Twilio webhooks and the media WebSocket, the per-call actor, business action execution, Postgres call history, metrics, and the admin API |
| `callora-providers` | ElevenLabs (TTS), Cartesia (streaming STT), OpenAI-compatible LLM (structured understanding), Twilio REST |
| `callora` | The binary: `serve`, `migrate`, `healthcheck`, `config validate`, `voice-library build/status`, `simulate`, `understand` |

## Endpoints

| Method | Path | |
| --- | --- | --- |
| POST | `/webhooks/twilio/voice` | Incoming call (Twilio-signed) → `<Connect><Stream>` |
| GET (WS) | `/webhooks/twilio/media` | Media stream, authorized by a call-bound token |
| POST | `/webhooks/twilio/call-status` | Call status callback (signed) |
| POST | `/webhooks/twilio/handoff-whisper` | Reads the collected context to the human agent (signed) |
| GET | `/health`, `/metrics` | Health, Prometheus metrics (latency histograms, audio-source mix) |
| GET | `/api/businesses`, `/api/calls`, `/api/calls/{id}` | Read-only, `X-Api-Key: $ADMIN_API_KEY` |

The webhook paths are the legacy ones, so numbers already configured in Twilio keep working.

## Configuration and secrets

Every variable and GitHub secret is listed, by name only, in [`SECRETS.md`](SECRETS.md).
Deployment is described in [`DEPLOYMENT.md`](DEPLOYMENT.md), and project status is in
[`docs/V2_STATUS.md`](docs/V2_STATUS.md).

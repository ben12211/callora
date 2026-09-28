# Callora V2

An AI phone agent for businesses. A business is described in a validated **Business JSON**
file, and Callora answers its phone: an LLM agent runs the conversation, the business's
fixed questions play from pre-recorded audio, and a generic engine enforces the business's
hard rules and performs its real actions.

```text
Incoming call → recorded greeting → streaming STT → the agent decides (streamed JSON)
→ values checked by the engine → recorded phrase | spoken sentence (library or live TTS)
→ read-back, yes, business action → telephone audio
```

This is deliberately not a speech-to-speech model: telephony, STT, the agent, state,
actions, audio, barge-in and handoff are separate parts that are each testable, and the
agent never acts on its own: nothing is sent without a read-back and a yes the caller
really said, and the call ends only on a real goodbye. The product specification is
[`docs/callora_voice_agent_architecture.md`](docs/callora_voice_agent_architecture.md); how
V2 implements it is in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md). The first business
is a taxi company: [`businesses/taxi.json`](businesses/taxi.json).

The previous TypeScript implementation is preserved on the `OLD-MAIN` branch. What V2
kept from it is listed in [`docs/LEGACY_INVENTORY.md`](docs/LEGACY_INVENTORY.md).

## Run it (Docker is the only requirement)

```bash
./dev test                 # full test suite (Rust toolchain + Postgres in containers)
./dev check                # rustfmt, clippy -D warnings, tests
./dev simulate taxi        # talk to the taxi business in text, through the real agent and engine
./dev callora eval         # the agent on recorded conversations, with the real model (evaluation/README.md)
./dev callora agent-prompt # the agent's system prompt and reply schema
./dev up                   # Postgres + server on http://localhost:3000
./dev logs / ./dev down / ./dev clean
```

Nothing is installed on the host. The toolchain, the cargo cache, build output and the
database all live in containers and named volumes. Behind a TLS-inspecting proxy, add a
git-ignored `docker-compose.local.yml` that mounts your CA into the `toolchain` and `app`
services (`./dev` picks it up automatically).

To make real calls: copy `.env.example` to `.env`, fill in the Twilio, Deepgram, ElevenLabs,
Gemini and OpenAI settings, expose port 3000 over HTTPS (for example with a tunnel), set
`PUBLIC_BASE_URL` and `TWILIO_SKIP_SIGNATURE_VALIDATION=false`, and point the Twilio
number's voice webhook at `https://<host>/webhooks/twilio/voice`. Generate the voice
library once (and again after adding responses):

```bash
./dev callora voice-library build --business taxi --out target/voice-library
```

## Before changing the agent

The agent's behaviour depends on its prompt (`crates/callora-core/src/agent.rs`), the
business's rules and examples (`businesses/taxi.json` → `agent`) and the model. Unit tests
cannot tell whether a model will still behave, so run the eval before and after:

```bash
./dev callora eval --model gpt-6-sol --model gpt-6-luna   # compare models
```

Every call that goes wrong becomes a case: mark it on `/calls`, export it, and write what
should have happened ([`evaluation/README.md`](evaluation/README.md)).

## Workspace

| Crate | What it is |
| --- | --- |
| `callora-core` | The generic runtime with no I/O: the Business JSON schema and validation, the agent's prompt and reply parsing, the deterministic understanding, explicit call state, the engine (tasks, slots, rules, read-back, fallback, handoff), response planning, Israel's places list, Hebrew numbers and spoken-text normalization |
| `callora-audio` | μ-law, VAD (barge-in + endpointing), the pre-generated voice library and its builder, the TTS cache, and paced, cancellable playout |
| `callora-runtime` | Twilio webhooks and the media WebSocket, the per-call actor (`session` with `hearing`, `agent_turn`, `speech`), business actions (with idempotency keys), Postgres call history, metrics, token pricing, and the owner's pages and admin API |
| `callora-providers` | OpenAI-compatible LLMs (the agent, with streaming and token usage; understanding), Deepgram (Nova-3 STT), ElevenLabs (TTS, Scribe STT), Cartesia (STT), Twilio REST, and the hedge/race between models |
| `callora` | The binary: `serve`, `migrate`, `healthcheck`, `config validate`, `voice-library build/status`, `simulate`, `understand`, `agent-prompt`, `eval` |

## Endpoints

| Method | Path | |
| --- | --- | --- |
| POST | `/webhooks/twilio/voice` | Incoming call (Twilio-signed) → `<Connect><Stream>` |
| GET (WS) | `/webhooks/twilio/media` | Media stream, authorized by a call-bound token |
| POST | `/webhooks/twilio/call-status` | Call status callback (signed) |
| POST | `/webhooks/twilio/handoff-whisper` | Reads the collected context to the human agent (signed) |
| GET | `/health`, `/metrics` | Health, Prometheus metrics (latency histograms, audio-source mix) |
| GET | `/calls`, `/orders` | The owner's pages: calls with their numbers, decisions, recordings and reviews; order cards. They ask for the admin key once. |
| GET | `/api/businesses`, `/api/calls`, `/api/calls/{id}`, `/api/orders`, `/api/stats?days=7` | `X-Api-Key: $ADMIN_API_KEY` |
| PUT | `/api/calls/{id}/review` | `{"verdict": "good" \| "bad", "note": "..."}` |
| GET | `/api/calls/{id}/eval-case`, `/api/utterances/{id}` | A call as an eval case; a recorded utterance as WAV |

The webhook paths are the legacy ones, so numbers already configured in Twilio keep working.

## Configuration and secrets

Every variable and GitHub secret is listed, by name only, in [`SECRETS.md`](SECRETS.md).
Deployment is described in [`DEPLOYMENT.md`](DEPLOYMENT.md), and project status is in
[`docs/V2_STATUS.md`](docs/V2_STATUS.md).

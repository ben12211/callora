# Hebrew voice pipeline

Callora's `deepdub` provider is the composable call path for low-latency Israeli Hebrew:

`Twilio μ-law/8000 → Cartesia multilingual STT → streaming text LLM → Hebrew speech frontend → Deepdub Phantom Z μ-law/8000 → Twilio`

The same call/session loop still owns interruption, Twilio marks and clears, transcript persistence, silence handling, hangup draining, metrics, and cleanup. OpenAI, ElevenLabs, and Cartesia remain separate selectable providers.

## Configuration

Required for Deepdub calls:

```dotenv
VOICE_PROVIDER=deepdub
DEEPDUB_API_KEY=dd-...
DEEPDUB_VOICE_ID=<voice-prompt-uuid-or-asset-id>
DEEPDUB_MODEL=dd-etts-3.0
DEEPDUB_LOCALE=he-IL
CARTESIA_API_KEY=...
OPENAI_API_KEY=...
```

Optional low-latency and privacy controls:

```dotenv
DEEPDUB_WS_URL=wss://wss.deepdub.ai/ws
DEEPDUB_FIRST_AUDIO_TIMEOUT_MS=50
DEEPDUB_ENABLE_LOGGING=false
RENIKUD_URL=http://renikud:8787
RENIKUD_TIMEOUT_MS=90
```

The Deepdub key stays in the server process and WebSocket handshake. Audio is requested as raw `mulaw` at `8000` Hz and forwarded to Twilio without decoding, resampling, or re-encoding.

## OFF, SMART, and STRICT

- `OFF` passes text directly to the selected TTS provider.
- `SMART` normalizes spoken-value context, applies the tenant dictionary, uses a tenant-keyed bounded in-memory cache, and calls ReNikudPlus only when deterministic risk signals indicate numbers, identifiers, dates, mixed scripts, acronyms, geresh/gershayim, abbreviations, or likely names.
- `STRICT` runs pronunciation assistance for every Hebrew chunk when the sidecar is configured. It is intended for evaluation and difficult terminology.

The dashboard exposes the mode and a per-business pronunciation editor. Dictionary queries and mutations always include `business_id`; the cache key also includes the business, locale, mode, and normalized text.

## ReNikudPlus sidecar

ReNikudPlus is optional and never owns call availability. Put the official `model_int8.onnx` and `renikud_onnx.py` files in `.models/renikud`, then run:

```bash
docker compose --profile renikud build renikud
RENIKUD_URL=http://renikud:8787 docker compose --profile renikud up --build
```

The Python process loads `G2P` once at startup. A timeout, connection error, non-2xx response, or malformed payload returns immediately to native Deepdub. There are no realtime retries.

## Streaming and barge-in

LLM deltas are grouped at Hebrew clause boundaries. Each safe chunk is preprocessed and sent immediately through Deepdub's persistent Streaming In/Streaming Out WebSocket while the LLM continues. `end-stream` closes the turn cleanly. On caller speech, Callora aborts the LLM, sends Deepdub `cancel`, drops obsolete provider audio, sends Twilio `clear`, clears pending marks, and does not release a new Deepdub turn until cancellation is acknowledged.

## Metrics

`/metrics` exports `callora_realtime_events_total` and `callora_realtime_stage_latency_ms`. Stages include pronunciation, LLM first token, TTS first audio, and the primary `speech_end_to_twilio_audio`. Prometheus histograms support p50/p95 queries without logging customer audio or full text.

## Evaluation and benchmark

`evaluation/hebrew-utterances.json` contains 110 categorized utterances. To run provider TTFA and pronunciation-overhead measurements:

```bash
pnpm install
pnpm benchmark:hebrew
```

Set `BENCHMARK_SAMPLES`, `BENCHMARK_MODES`, and optionally `HEBREW_EVAL_OUTPUT_DIR`. With an output directory, the utility writes raw μ-law files for blind listening. Play one with:

```bash
ffplay -f mulaw -ar 8000 -ac 1 001-smart.ulaw
```

Rate naturalness, Israeli pronunciation, contextual correctness, prosody, and artifacts without showing the mode to listeners. The benchmark reports median and p95 separately for pronunciation preprocessing, provider TTFA, and their combined latency. Full speech-end-to-first-audio is measured on live calls through `/metrics` because a standalone TTS benchmark has no caller speech-end event.

## Local verification

```bash
pnpm install
pnpm db:migrate
pnpm test
pnpm typecheck
pnpm lint
pnpm build
pnpm dev
```

# What V2 kept from the legacy implementation

The legacy implementation (Node/TypeScript: a speech-to-speech bridge to OpenAI Realtime,
ElevenLabs Agents, Cartesia and Deepdub, with a server-rendered dashboard) is preserved on
`OLD-MAIN` at commit `bb8d327`. V2 kept its **integration knowledge and operational
conventions**, not its architecture or code.

## Kept (re-implemented in Rust)

| Area | Legacy knowledge | Where in V2 |
| --- | --- | --- |
| Twilio webhooks | `/webhooks/twilio/voice`, `/call-status`, `/media` paths; signature over `PUBLIC_BASE_URL` + path; `From` never used for routing | `callora-runtime/src/twilio.rs`, `server.rs` |
| Media stream auth | Short-lived HMAC token bound to CallSid + business, passed as a `<Parameter>`, multiple secrets accepted for rotation, `STREAM_TOKEN_SECRET` separate from the Twilio token | `twilio.rs` |
| Twilio REST | Hang up with `Status=completed`; 404 / 20404 / 20009 / 21220 mean "already over" | `callora-providers/src/twilio_rest.rs` |
| Audio | μ-law 8 kHz end to end, no transcoding; 20 ms paced frames; `clear` on barge-in | `callora-audio` |
| VAD | Energy threshold ~-31 dBFS; quiet frames drain twice as fast as speech fills | `callora-audio/src/vad.rs` |
| Cartesia STT | `ink-whisper` (multilingual; `ink-2` is English-only), `pcm_mulaw`/8000, key only in the `X-API-Key` header, finals are deltas, a short service-side silence split Hebrew sentences | `callora-providers/src/cartesia.rs` (V2 endpoints locally with VAD + `finalize`) |
| ElevenLabs | API origin/key variables; `v3` models for Hebrew (the fast v2 models produced nonsense); phone voice settings (stability, similarity 0.75) | `callora-providers/src/elevenlabs.rs`, `businesses/taxi.json` |
| Hebrew speech | Money, percentages, Israeli phone patterns, times, dates, order numbers normalized before TTS; business pronunciation dictionary; the 110-utterance evaluation corpus | `callora-core/src/speech.rs`, `evaluation/hebrew-utterances.json` (now a test) |
| Caller allowlist | `ALLOW_LIST` with the same loose parsing | `callora/src/main.rs` |
| Transcript retention | `TRANSCRIPT_RETENTION_DAYS`, default 30 | `callora-runtime/src/store.rs` |
| Deployment | Oracle Linux 9 ARM64 VM, Docker Hub private image by commit SHA, Caddy HTTPS + fallback upstream, Postgres volume never recreated, disk reclaim, atomic `.env`, port-conflict checks, rollback, pinned GitHub Actions, `production` environment | `deploy/`, `Caddyfile`, `docker-compose.prod.yml`, `.github/workflows/ci-cd.yml` |
| Secret names | `IP`, `USER`, `KEY_PEM`, `DOCKER_HUB_*`, `TWILIO_*`, `CARTESIA_API_KEY`, `ELEVENLABS_API_KEY`, `OPENAI_API_KEY`, `ALLOW_LIST`, `ADMIN_API_KEY`, `TEXT_LLM_*` | `SECRETS.md` |

## Not carried over

- The speech-to-speech bridges and provider switching (`VOICE_PROVIDER`): V2 is a
  composed pipeline with its own state, as the architecture MD requires.
- Deepdub and the ReNikud pronunciation sidecar: V2 pre-generates most audio and uses a
  deterministic normalizer plus a pronunciation dictionary.
- The prompt-based agent policy: behaviour now comes from validated business config and
  the engine, not from instructions to a model.
- The multi-tenant dashboard, admin users and encrypted credentials store (`SECRETS_KEY`,
  `ADMIN_EMAIL/PASSWORD`): V2 ships a read-only API; a management UI can be built on top
  of it later.
- The `public` schema tables: untouched in production and not read by V2.

No phone numbers, provider ids or secret values were found committed in the legacy code
(only placeholders); real values were always in GitHub and the VM `.env`.

# Callora V2 — secrets and settings inventory

**Names only.** No value for any of these belongs in the repository, a commit, a log, a
test or an issue. Real values live in GitHub (Repository Secrets / Variables), in
`/opt/callora/.env` on the production VM, or in a local, git-ignored `.env`.

Names marked *legacy* are unchanged from the previous implementation (`OLD-MAIN`), so the
secrets already configured in GitHub keep working.

## GitHub Repository Secrets

| Name | Required | Used for |
| --- | --- | --- |
| `IP` | yes | Production VM public IPv4 address (SSH target). *legacy* |
| `USER` | yes | SSH deployment account on the VM. *legacy* |
| `KEY_PEM` | yes | Private SSH key for `USER` (unencrypted, OpenSSH/PEM format). *legacy* |
| `SSH_KNOWN_HOSTS` | recommended | The VM's pinned host key (`ssh-keyscan -t ed25519 <IP>` output). Unset → trust on first use, as before. |
| `DOCKER_HUB_TOKEN` | yes | Pushes and pulls the private `callora` image. *legacy* |
| `TWILIO_ACCOUNT_SID` | yes | Twilio REST: hang up, transfer to a human. *legacy* |
| `TWILIO_AUTH_TOKEN` | yes | Validates every Twilio webhook signature; signs media-stream tokens when `STREAM_TOKEN_SECRET` is unset. *legacy* |
| `STREAM_TOKEN_SECRET` | recommended | Separate key for media-stream tokens (≥16 chars), so rotating the Twilio token does not change it. *legacy name* |
| `CARTESIA_API_KEY` | optional | Backup speech-to-text (Cartesia `ink-whisper`), used with `STT_PROVIDER=cartesia` or when there is no ElevenLabs key. *legacy* |
| `ELEVENLABS_API_KEY` | yes | Speech-to-text (Scribe v2 Realtime), voice library generation and dynamic TTS. Without it calls cannot be understood. *legacy* |
| `ELEVENLABS_VOICE_ID` | yes for audio | The business voice (also accepted as a Variable, which is preferred). |
| `GEMINI_API_KEY` | recommended | LLM structured understanding with Gemini (default model `gemini-3.8-flash`). With `OPENAI_API_KEY` too, both are asked and the first valid answer wins. A free-tier key is rate limited to a few requests a minute. |
| `OPENAI_API_KEY` | recommended | LLM structured understanding when the fast path is unsure. Without it only the deterministic path runs. *legacy* |
| `TAXI_PHONE_NUMBERS` | yes | Comma-separated E.164 Twilio numbers the taxi business answers on (also accepted as a Variable). |
| `TAXI_HANDOFF_NUMBER` | recommended | E.164 number of the human dispatch desk. Without it, handoff says no one is available (also accepted as a Variable). |
| `TAXI_DISPATCH_URL` | optional | The taxi company's dispatch endpoint. Unset → demo (mock) results. |
| `TAXI_DISPATCH_TOKEN` | optional | Bearer token for `TAXI_DISPATCH_URL`. |
| `TAXI_CRM_URL` | optional | Customer lookup endpoint (by caller number). Unset → callers are unknown. |
| `TAXI_CRM_TOKEN` | optional | Bearer token for `TAXI_CRM_URL`. |
| `ALLOW_LIST` | optional | Comma-separated E.164 callers allowed to reach the agent; empty allows everyone. *legacy* |
| `ADMIN_API_KEY` | optional | `X-Api-Key` for the read-only `/api` (≥16 chars). *legacy* |

## GitHub Repository Variables

| Name | Required | Used for |
| --- | --- | --- |
| `DOCKER_HUB_USERNAME` | yes | Docker Hub namespace of the image. *legacy* |
| `PUBLIC_BASE_URL` | yes for a fresh VM | `https://<hostname>` Twilio calls, no trailing slash. Written into the VM's `.env` only when absent there (never overwritten). The hostname's DNS A record must point at the VM. |
| `ELEVENLABS_VOICE_ID` | yes for audio | Preferred place for the voice id (not sensitive). |
| `ELEVENLABS_DYNAMIC_MODEL` | optional | Overrides the business's dynamic TTS model. |
| `TEXT_LLM_MODEL` | optional | OpenAI-side model for understanding (default `gpt-4o-mini`). *legacy* |
| `GEMINI_MODEL` | optional | Gemini model for understanding (default `gemini-3.8-flash`). |
| `TEXT_LLM_REASONING_EFFORT` | optional | Gemini thinking level, sent as `reasoning_effort` (default `low`; `gemini-3.8-flash` rejects `minimal`). |
| `STT_PROVIDER` | optional | `scribe` (default) or `cartesia`. |
| `TAXI_PHONE_NUMBERS`, `TAXI_HANDOFF_NUMBER` | see above | May be Variables instead of Secrets. |

## Host-only settings (`/opt/callora/.env` on the VM, never sent by the pipeline)

`POSTGRES_USER`, `POSTGRES_PASSWORD`, `POSTGRES_DB`, `DATABASE_URL`, `PUBLIC_BASE_URL` — *legacy*, unchanged.
On a fresh VM, `deploy.sh init-host` (run by CI) creates them: the database password is
generated on the VM and never leaves it, and `PUBLIC_BASE_URL` comes from the Variable above.

## Optional runtime tuning (environment)

`RUST_LOG`, `LOG_FORMAT`, `HOST`, `PORT`, `BUSINESS_CONFIG_DIR`, `AUDIO_LIBRARY_DIR`,
`TRANSCRIPT_RETENTION_DAYS` (*legacy*), `TTS_CACHE_ENTRIES`, `VAD_TRIGGER_MS`,
`VAD_ENDPOINT_MS`, `ELEVENLABS_API_BASE_URL` (*legacy*), `ELEVENLABS_LIBRARY_MODEL`,
`CARTESIA_STT_URL`, `CARTESIA_STT_MODEL` (*legacy*), `CARTESIA_VERSION` (*legacy*),
`TEXT_LLM_BASE_URL` (*legacy*), `TWILIO_SKIP_SIGNATURE_VALIDATION` (local development only).

## Retired with the legacy implementation

No longer read by V2; safe to delete from GitHub once `OLD-MAIN` is no longer deployed:
`VOICE_PROVIDER`, `ELEVENLABS_AGENT_ID`, `CARTESIA_VOICE_ID`, `DEEPDUB_API_KEY`,
`DEEPDUB_VOICE_ID`, `RENIKUD_URL`, `SECRETS_KEY`, `ADMIN_EMAIL`, `ADMIN_PASSWORD`.

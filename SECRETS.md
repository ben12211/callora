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
| `DEEPGRAM_API_KEY` | recommended | Backup speech-to-text (Deepgram Nova-3, Hebrew), used when OpenAI's recognition fails or with `STT_PROVIDER=deepgram`. |
| `ELEVENLABS_API_KEY` | yes | Voice library generation and dynamic TTS. *legacy* |
| `ELEVENLABS_VOICE_ID` | yes for audio | The business voice (also accepted as a Variable, which is preferred). |
| `OPENAI_API_KEY` | yes | The agent's backup model (`gpt-6-luna`), OpenAI agent models, and understanding for businesses without an agent. *legacy* |
| `GEMINI_API_KEY` | yes | The conversation agent (default `gemini-3.8-flash`). A free-tier key is rate limited to a few requests a minute: use a paid one. Also understanding for businesses without an agent. |
| `TAXI_PHONE_NUMBERS` | yes | Comma-separated E.164 Twilio numbers the taxi business answers on (also accepted as a Variable). |
| `TAXI_HANDOFF_NUMBER` | recommended | E.164 number of the human dispatch desk. Without it, handoff says no one is available (also accepted as a Variable). |
| `TAXI_DISPATCH_URL` | optional | The taxi company's dispatch endpoint. Unset → demo (mock) results. |
| `TAXI_DISPATCH_TOKEN` | optional | Bearer token for `TAXI_DISPATCH_URL`. |
| `TAXI_CRM_URL` | optional | Customer lookup endpoint (by caller number). Unset → callers are unknown. |
| `TAXI_CRM_TOKEN` | optional | Bearer token for `TAXI_CRM_URL`. |
| `ALLOW_LIST` | optional | Comma-separated E.164 callers allowed to reach the agent; empty allows everyone. *legacy* |
| `DASHBOARD_PASSWORD` | yes | The dashboard's password, 8 characters or more. Unset, the legacy `ADMIN_PASSWORD` is used; with neither, nobody can sign in (an error is logged). Changing it signs everyone out. |
| `ADMIN_API_KEY` | optional | `X-Api-Key` for the read-only `/api` (≥16 chars). *legacy* |

## GitHub Repository Variables

| Name | Required | Used for |
| --- | --- | --- |
| `DOCKER_HUB_USERNAME` | yes | Docker Hub namespace of the image. *legacy* |
| `PUBLIC_BASE_URL` | yes for a fresh VM | `https://<hostname>` Twilio calls, no trailing slash. Written into the VM's `.env` only when absent there (never overwritten). The hostname's DNS A record must point at the VM. |
| `ELEVENLABS_VOICE_ID` | yes for audio | Preferred place for the voice id (not sensitive). |
| `ELEVENLABS_DYNAMIC_MODEL` | optional | Overrides the business's dynamic TTS model. |
| `AGENT_MODEL` | optional | The agent's model (default `gemini-3.8-flash`, needs `GEMINI_API_KEY`; a non-`gemini-*` name goes to OpenAI). Sent by the pipeline: unset here means the default, even if the VM's `.env` said otherwise. Run the eval before changing it. A model chosen on the dashboard's settings page (stored in `callora_v2.app_settings`) wins over this until "back to default" is pressed there. |
| `AGENT_BACKUP_MODEL` | optional | The hedge: asked when the agent's model has said nothing after `AGENT_HEDGE_MS` (default `gpt-6-luna`). |
| `AGENT_REASONING_EFFORT` | optional | How much a reasoning model thinks before its first word: `none` (default), `low`, `medium`. Each step up is slower on the phone. |
| `AGENT_PRICES` | optional | Dollars per million tokens as `model=input/cached/output`, comma separated (`gpt-6-sol=2/0.2/10,gpt-6-luna=0.1/0.01/0.5`), for the cost per call on `/calls` and in the eval. Without it cost is shown as unknown. |
| `AUDIO_SAMPLE_NUMBERS` | optional | Comma-separated E.164 callers (the owner's test phones) whose utterances are kept as audio, for listening on `/calls` and comparing recognizers. Deleted with transcripts after `TRANSCRIPT_RETENTION_DAYS`. |
| `TEXT_LLM_MODEL` | optional | OpenAI-side model for understanding without an agent (default `gpt-4o-mini`). *legacy* |
| `GEMINI_MODEL` | optional | Gemini model for understanding (default `gemini-3.8-flash`). |
| `TEXT_LLM_REASONING_EFFORT` | optional | Gemini thinking level, sent as `reasoning_effort` (default `low`; `gemini-3.8-flash` rejects `minimal`). |
| `STT_PROVIDER` | optional | `openai` (default, backed by Deepgram), `deepgram`, or `soniox` (backed by OpenAI). |
| `SONIOX_API_KEY` | optional | Soniox real-time speech-to-text (`stt-rt-v5`), for `STT_PROVIDER=soniox`; the eval also runs the voice cases through it for comparison. |
| `TAXI_PHONE_NUMBERS`, `TAXI_HANDOFF_NUMBER` | see above | May be Variables instead of Secrets. |

## Host-only settings (`/opt/callora/.env` on the VM, never sent by the pipeline)

`POSTGRES_USER`, `POSTGRES_PASSWORD`, `POSTGRES_DB`, `DATABASE_URL`, `PUBLIC_BASE_URL` — *legacy*, unchanged.
On a fresh VM, `deploy.sh init-host` (run by CI) creates them: the database password is
generated on the VM and never leaves it, and `PUBLIC_BASE_URL` comes from the Variable above.

## WhatsApp (host-only)

`WHATSAPP_TOKEN`, the secret between the backend and the WhatsApp service, is created on the
VM by `deploy.sh init-host` and never leaves it. `WHATSAPP_MAX_SESSIONS` (default 5) caps the
accounts: each one runs a browser of its own (about 300–500 MB).

## Optional runtime tuning (environment)

`AGENT_HEDGE_MS` (default `1100`), `AGENT_SPECULATE` (the agent starts on the partial transcript at the end of speech; on unless `false`),
`SECOND_HEARING` (a street or city answer the stream got wrong is heard again by an audio model told the names expected; `off` turns it off), `SECOND_HEARING_MODEL` (default `gpt-audio-1.5`),
`EVAL_PRICES` (prices for `callora eval` only; defaults to `AGENT_PRICES`),
`RUST_LOG`, `LOG_FORMAT`, `HOST`, `PORT`, `BUSINESS_CONFIG_DIR`, `AUDIO_LIBRARY_DIR`,
`TRANSCRIPT_RETENTION_DAYS` (*legacy*), `TTS_CACHE_ENTRIES`, `VAD_TRIGGER_MS`,
`VAD_ENDPOINT_MS`, `ELEVENLABS_API_BASE_URL` (*legacy*), `ELEVENLABS_LIBRARY_MODEL`,
`DEEPGRAM_STT_URL`, `DEEPGRAM_STT_MODEL` (default `nova-3`),
`TEXT_LLM_BASE_URL` (*legacy*), `TWILIO_SKIP_SIGNATURE_VALIDATION` (local development only).

## Retired with the legacy implementation

No longer read by V2; safe to delete from GitHub once `OLD-MAIN` is no longer deployed:
`VOICE_PROVIDER`, `ELEVENLABS_AGENT_ID`, `CARTESIA_API_KEY`, `CARTESIA_VOICE_ID`, `DEEPDUB_API_KEY`,
`DEEPDUB_VOICE_ID`, `RENIKUD_URL`, `SECRETS_KEY`, `ADMIN_EMAIL`, `ADMIN_PASSWORD`.

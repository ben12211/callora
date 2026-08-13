# Changelog

Shared record of meaningful completed Callora changes.

## Unreleased

### Added

- Bidirectional Twilio Media Streams (`<Connect><Stream>`) to OpenAI Realtime (`gpt-realtime-2.1`) speech-to-speech calls, bridging G.711 mu-law audio in both directions without transcoding, with server-VAD barge-in and an AI-spoken greeting.
- Token-authenticated `/webhooks/twilio/media` WebSocket endpoint, call-scoped and business-scoped, with `CallSid` verification on the stream `start` event.
- Per-business `agent_configs` (instructions, greeting, language, voice, realtime model, enabled) plus a Hebrew customer-service agent for the demo business.
- `twilio_stream_sid` and `openai_session_id` on call records, structured realtime lifecycle logs, and `OPENAI_API_KEY` configuration across `.env`, Compose, and the deployment secret sync.

### Changed

- The voice webhook now answers with `<Connect><Stream>` when the resolved business has an enabled agent; businesses without one keep the static `<Say>` greeting.
- Production CI/CD now publishes private ARM64 images to Docker Hub and deploys immutable commit-SHA releases with the existing health-gated rollback flow.

### Added

- Minimal shared instructions for Codex, Claude Code, and GitHub Copilot.
- Canonical task-specific Agent Skills under `.agents/skills/` with upstream licenses.
- Concise, updateable architecture and project state in `docs/AI_STATE.md`.

## 0.1.0 - 2026-08-13

### Added

- Multi-tenant Twilio/PostgreSQL backend foundation with signed webhooks, tenant greetings, business CRUD, call reads, tests, and health checks.
- ARM64 Docker/Caddy production deployment, GHCR CI/CD, Oracle Linux bootstrap, health-gated releases, and safe application rollback.

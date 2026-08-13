# Changelog

Shared record of meaningful completed Callora changes.

## Unreleased

### Changed

- Production CI/CD now publishes private ARM64 images to Docker Hub and deploys immutable commit-SHA releases with the existing health-gated rollback flow.

### Added

- Minimal shared instructions for Codex, Claude Code, and GitHub Copilot.
- Canonical task-specific Agent Skills under `.agents/skills/` with upstream licenses.
- Concise, updateable architecture and project state in `docs/AI_STATE.md`.

## 0.1.0 - 2026-08-13

### Added

- Multi-tenant Twilio/PostgreSQL backend foundation with signed webhooks, tenant greetings, business CRUD, call reads, tests, and health checks.
- ARM64 Docker/Caddy production deployment, GHCR CI/CD, Oracle Linux bootstrap, health-gated releases, and safe application rollback.

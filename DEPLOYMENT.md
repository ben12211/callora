# Callora production deployment

Callora deploys as three containers on one Oracle Linux 9 ARM64 VM:

- Caddy terminates HTTPS on ports 80/443 and proxies to the backend.
- The non-root Callora backend is available only inside the Docker network.
- PostgreSQL is available only inside its private Docker network and stores data in the fixed `callora_postgres_data` named volume.

The production stack is defined in `docker-compose.prod.yml`. Normal deployments never run `docker compose down`, never use `--volumes`, and never recreate or delete the PostgreSQL volume.

## Before the first deployment

1. Point the hostname used by `PUBLIC_BASE_URL` to the VM's public IP.
2. Allow inbound TCP 80 and 443 in the Oracle Cloud VCN security list or network security group. Do not expose 3000 or 5432.
3. Bootstrap the VM as described below.
4. Add the GitHub `production` environment and its secrets.
5. If desired, add required reviewers to the `production` environment.

## Bootstrap Oracle Linux 9

Run these commands while logged into the VM as the intended deployment user (normally `opc`):

```bash
curl -fsSL https://raw.githubusercontent.com/ben12211/callora/main/deploy/bootstrap-oracle-linux.sh \
  -o /tmp/bootstrap-callora.sh
chmod 0755 /tmp/bootstrap-callora.sh
sudo /tmp/bootstrap-callora.sh "$(id -un)"
exit
```

Reconnect so the new Docker group membership applies, then verify it:

```bash
docker version
docker compose version
test -w /opt/callora
```

The bootstrap script uses Docker's RPM repository, installs Docker Engine plus the Compose/Buildx plugins, enables Docker at boot, creates `/opt/callora`, and opens HTTP/HTTPS in `firewalld`. It does not alter Oracle Cloud VCN or NSG rules.

If the repository is private and the raw download is unavailable, copy `deploy/bootstrap-oracle-linux.sh` to `/tmp/bootstrap-callora.sh` over SCP and run the same `chmod` and `sudo` commands.

## GitHub Secrets

Create these as secrets in the GitHub `production` environment:

| Secret | Value |
| --- | --- |
| `SERVER_HOST` | VM hostname or public IP |
| `SERVER_USER` | SSH deployment user, normally `opc` |
| `SERVER_SSH_KEY` | Private SSH key authorized for `SERVER_USER` |
| `SERVER_SSH_PORT` | SSH port, normally `22` |
| `SERVER_SSH_KNOWN_HOSTS` | Verified `known_hosts` line for the VM |
| `POSTGRES_USER` | PostgreSQL role, for example `callora` |
| `POSTGRES_PASSWORD` | Strong PostgreSQL password |
| `POSTGRES_DB` | PostgreSQL database, for example `callora` |
| `DATABASE_URL` | Container URL using host `db`, for example `postgresql://callora:URL_ENCODED_PASSWORD@db:5432/callora` |
| `TWILIO_ACCOUNT_SID` | Twilio account SID |
| `TWILIO_AUTH_TOKEN` | Twilio Auth Token used for webhook validation |
| `PUBLIC_BASE_URL` | HTTPS origin with no path, for example `https://calls.example.com` |

Generate and verify the SSH host-key entry from a trusted machine before saving it:

```bash
ssh-keyscan -p 22 your-server.example.com
ssh-keygen -lf /path/to/the/server-host-public-key
```

Use the `ssh-keyscan` output only after comparing its fingerprint with the host key on the VM. The workflow uses strict host-key checking and does not retrieve keys dynamically.

`GITHUB_TOKEN` is supplied automatically by GitHub Actions. The image job receives only `packages: write`; the deployment job receives only `packages: read`. The temporary token is used to pull from GHCR and is logged out on the VM when deployment finishes.

If `POSTGRES_PASSWORD` contains URL-reserved characters, percent-encode the password portion in `DATABASE_URL`. The unencoded value remains in `POSTGRES_PASSWORD`.

## CI/CD behavior

Pull requests run dependency installation, lint, tests, and the TypeScript build without access to production secrets.

A successful push to `main` then:

1. Builds `linux/arm64` with Buildx/QEMU.
2. Pushes `ghcr.io/ben12211/callora:<commit-sha>` and `ghcr.io/ben12211/callora:latest`.
3. Copies only the production Compose file, Caddyfile, deploy script, and a generated mode-0600 environment file to `/opt/callora`.
4. Pulls the immutable SHA image.
5. Starts or verifies PostgreSQL without replacing its named volume.
6. Runs advisory-locked migrations and the idempotent seed before replacing the backend.
7. Waits for database, backend, Caddy, internal `/health`, and public `/health` checks.
8. Confirms the release only after every health gate passes.

Only push events on `main` can publish or deploy. Main deployments are serialized to keep the `latest` tag and production release ordered.

## Rollback behavior

Before changing the running application, `/opt/callora/deploy.sh` saves the prior image reference and production configuration. If a migration, container, or health check fails, it restores the previous backend and Caddy configuration. On a failed first deployment it stops the app containers but preserves PostgreSQL and its volume.

Database migrations are never automatically reversed because doing so could destroy data. New migrations must therefore be backward-compatible with the previous application image. The old backend remains running while the new image is pulled and migrations execute; only the final single-container replacement creates a brief application restart.

Useful production diagnostics:

```bash
cd /opt/callora
docker compose --env-file .env -f docker-compose.prod.yml ps
docker compose --env-file .env -f docker-compose.prod.yml logs --tail=200 backend
docker compose --env-file .env -f docker-compose.prod.yml logs --tail=200 caddy
docker volume inspect callora_postgres_data
```

Do not run `docker compose down --volumes` in production.

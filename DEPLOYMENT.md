# Callora production deployment

Callora deploys as three containers on one Oracle Linux 9 ARM64 VM:

- Caddy terminates HTTPS on ports 80/443 and proxies to the backend.
- The non-root Callora backend is available only inside the Docker network.
- PostgreSQL is available only inside its private Docker network and stores data in the fixed `callora_postgres_data` named volume.

The production stack is defined in `docker-compose.prod.yml`. Normal deployments never run `docker compose down`, never use `--volumes`, and never recreate or delete the PostgreSQL volume.

## One-time Docker Hub setup

Under the Docker Hub account or organization named by `DOCKER_HUB_USERNAME`:

1. Create a repository named `callora`.
2. Set its visibility to **Private**.
3. Create an access token that can read and write that repository. Delete permission is not needed.
4. Store the token only in the GitHub Repository Secret `DOCKER_HUB_TOKEN`.

The resulting image name is `DOCKER_HUB_USERNAME/callora`. The workflow publishes both the immutable commit SHA tag and the convenience `latest` tag, but production always deploys the SHA tag.

## Before the first deployment

1. Point the hostname used by `PUBLIC_BASE_URL` to the VM's public IP.
2. Allow inbound TCP 80 and 443 in the Oracle Cloud VCN security list or network security group. Keep 3000 and 5432 closed. SSH must be available on port 22.
3. Bootstrap the VM as described below.
4. Authorize the public key matching `KEY_PEM` for the deployment account named by `USER`.
5. Create `/opt/callora/.env` with the production application settings shown below.
6. Create the exact GitHub Repository Secrets and Variable listed below.

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

## Production application environment on the VM

The database credentials and public URL stay on the VM. Twilio and OpenAI credentials are synchronized from GitHub Repository Secrets during deployment. As the deployment user, create `/opt/callora/.env` with mode `0600`:

```dotenv
NODE_ENV=production
HOST=0.0.0.0
PORT=3000
LOG_LEVEL=info
POSTGRES_USER=callora
POSTGRES_PASSWORD=replace-with-a-strong-password
POSTGRES_DB=callora
DATABASE_URL=postgresql://callora:URL_ENCODED_PASSWORD@db:5432/callora
TWILIO_ACCOUNT_SID=ACxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
TWILIO_AUTH_TOKEN=replace-with-your-auth-token
PUBLIC_BASE_URL=https://calls.example.com
OPENAI_API_KEY=replace-with-your-openai-api-key
```

Then secure it:

```bash
chmod 0600 /opt/callora/.env
```

If `POSTGRES_PASSWORD` contains URL-reserved characters, percent-encode the password portion in `DATABASE_URL`. The unencoded value remains in `POSTGRES_PASSWORD`. `DATABASE_URL` must use the Compose service hostname `db`.

Do not add `CALLORA_IMAGE` manually. During deployment, `deploy.sh` copies the existing server environment to a mode-0600 release file and inserts only the exact commit-SHA image. Rollback restores the previous environment and image reference.

## Required GitHub repository settings

Create these under **Settings → Secrets and variables → Actions** for the repository.

Repository Secrets:

| Secret | Value |
| --- | --- |
| `DOCKER_HUB_TOKEN` | Docker Hub access token with read/write access to the private `callora` repository |
| `IP` | Oracle VM public IPv4 address |
| `KEY_PEM` | Private SSH key authorized for `USER` |
| `USER` | SSH deployment account, normally `opc` |
| `TWILIO_ACCOUNT_SID` | Twilio Account SID; authenticates the REST call that hangs up finished conversations |
| `TWILIO_AUTH_TOKEN` | Twilio Auth Token used to validate signed webhook requests |
| `OPENAI_API_KEY` | OpenAI API key with Realtime access, used for the speech-to-speech call bridge |

Repository Variable:

| Variable | Value |
| --- | --- |
| `DOCKER_HUB_USERNAME` | Docker Hub account or organization that owns the private `callora` repository |

No GHCR credentials, `GITHUB_TOKEN` package permissions, GitHub environment secrets, or database secrets are used by the workflow. The deploy job continues to target the existing `production` environment so any protection rules or required reviewers remain in force; its credentials still come only from the Repository Secrets above. Twilio credentials are sent to the VM over the existing SSH connection through standard input and are never printed or included in a remote command line.

The workflow obtains the VM's current SSH host key with `ssh-keyscan`, stores it only in the ephemeral runner, and then enables strict host-key checking for SSH and SCP. Because the requested GitHub configuration does not include a separately trusted host-key fingerprint, that initial scan is not independently authenticated. The VM address and private key remain masked GitHub Secrets, and credentials are never printed or passed as command-line passwords.

## CI/CD behavior

Pull requests run dependency installation, deployment configuration validation, lint, tests, and the TypeScript build. They never log in to Docker Hub, publish an image, use deployment secrets, or deploy.

A successful push to `main` then:

1. Runs the same lint, tests, build, shell syntax, YAML, and Compose configuration checks.
2. Builds a `linux/arm64` image with Buildx/QEMU.
3. Logs in to Docker Hub without printing the token.
4. Pushes `DOCKER_HUB_USERNAME/callora:<commit-sha>` and `DOCKER_HUB_USERNAME/callora:latest`.
5. Connects to `USER@IP` over SSH on port 22.
6. Logs in to Docker Hub on the VM with `DOCKER_HUB_USERNAME` and `DOCKER_HUB_TOKEN` over standard input.
7. Copies only the production Compose file, Caddyfile, and deploy script to `/opt/callora`.
8. Injects the exact SHA image into a copy of the existing server environment and pulls that image.
9. Starts or verifies PostgreSQL without replacing its fixed named volume.
10. Validates Caddy and runs advisory-locked, transactional migrations plus the idempotent seed before replacing the backend.
11. Deploys the existing backend/PostgreSQL/Caddy Compose stack.
12. Waits for PostgreSQL, backend, Caddy, internal backend `/health`, and public HTTPS `/health` checks.
13. Confirms the release only after every health gate passes, then logs out of Docker Hub on the VM.

The image and deploy jobs run only for push events on `main`, depend on the successful quality job, and are serialized per branch. Production is always deployed by commit SHA, never by `latest`.

## Rollback behavior

Before changing the running application, `/opt/callora/deploy.sh` saves the prior image reference and production configuration. If image pull, Caddy validation, migration, container startup, or any health check fails, it restores the previous backend and Caddy configuration. On a failed first deployment it stops the app containers, restores the original server environment, and preserves PostgreSQL and its volume.

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

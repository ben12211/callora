# Callora production deployment

Callora V2 deploys the same way the legacy stack did: three containers on one Oracle Linux 9 ARM64 VM:

- Caddy terminates HTTPS on ports 80/443 and proxies to the backend.
- The non-root, distroless Callora backend (a single static Rust binary) is available only inside the Docker network. It keeps its pre-generated voice library in the `callora_voice_library` named volume.
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

No manual work happens on the VM. The deploy workflow bootstraps it itself, over the same SSH connection it deploys with.

1. **Oracle Cloud** (console, not the VM): allow inbound TCP 22, 80 and 443 in the VCN security list or NSG. Keep 3000 and 5432 closed.
2. **DNS:** point the `PUBLIC_BASE_URL` hostname's A record at the VM.
3. **GitHub:** create the Secrets and Variables in [SECRETS.md](SECRETS.md). At minimum: `IP`, `USER` (`opc`), `KEY_PEM`, `DOCKER_HUB_TOKEN`, `DOCKER_HUB_USERNAME`, `PUBLIC_BASE_URL`, `TWILIO_ACCOUNT_SID`, `TWILIO_AUTH_TOKEN`, `TAXI_PHONE_NUMBERS`, `CARTESIA_API_KEY`, `ELEVENLABS_API_KEY`, `ELEVENLABS_VOICE_ID`. Pinning the host key in `SSH_KNOWN_HOSTS` is recommended.
4. **Run the workflow.** Push to `main`, or, from **Actions → Callora CI/CD → Run workflow** on `main`:
   - `bootstrap`: prepare and validate the VM only;
   - `deploy`: a full release;
   - `reset` with `confirm_reset = DELETE-LEGACY-CALLORA`: remove the legacy Callora deployment, then bootstrap and deploy V2 on a clean host.

What the **host** job does on every run (idempotent):

- `deploy/ci-ssh-setup.sh` checks `KEY_PEM`, sets up the host key, and proves SSH works.
- `deploy/bootstrap-oracle-linux.sh` (via `sudo -n`) installs Docker Engine and the Compose plugin only if they are missing, configures log rotation, adds `USER` to the `docker` group, creates `/opt/callora`, and opens HTTP/HTTPS in firewalld. It never restarts a Docker daemon that is already serving.
- `deploy.sh purge-legacy` runs **only in `reset` mode**. It removes containers of the Compose project `callora`, `*/callora:*` and `callora-renikud` images, the `callora_postgres_data` and `callora_voice_library` volumes, the `callora_*` networks, and Callora's files in `/opt/callora`. It keeps Caddy's certificate volumes, and anything belonging to other stacks on the VM (such as the service behind Caddy's `:80` fallback). Other Callora-named leftovers are only reported.
- `deploy.sh init-host` creates any missing host settings: `PUBLIC_BASE_URL` from the Variable, and a database password generated on the VM with the matching `DATABASE_URL`. It never overwrites an existing value, and it refuses to invent a password next to an existing database volume. It also verifies that the deploy user can use Docker.

The **deploy** job then syncs runtime settings, pulls the SHA-tagged image, runs migrations, replaces the backend, starts Caddy, waits for the container, internal and public HTTPS health checks, and rolls back automatically if anything fails.

### Running the reset from a workstation (only if the workflow cannot run yet)

The same scripts, from PowerShell on the machine that holds the key (replace the IP and key path):

```powershell
$key = "C:\path\to\ssh-key.key"; $vm = "opc@<IP>"
scp -i $key deploy/bootstrap-oracle-linux.sh deploy/deploy.sh "${vm}:/tmp/"
ssh -i $key $vm "sudo -n bash /tmp/bootstrap-oracle-linux.sh opc && install -m 0755 /tmp/deploy.sh /opt/callora/deploy.sh"
ssh -i $key $vm "bash /opt/callora/deploy.sh purge-legacy I-UNDERSTAND-THIS-DELETES-THE-CALLORA-DATABASE"
"PUBLIC_BASE_URL=https://<hostname>" | ssh -i $key $vm "bash /opt/callora/deploy.sh init-host"
```

The first real release still comes from the workflow (it builds and publishes the image).

## Production environment on the VM

The database credentials and the public URL stay on the VM. Everything else is synchronized from GitHub on each deployment (see below). Create `/opt/callora/.env` with mode `0600`:

```dotenv
POSTGRES_USER=callora
POSTGRES_PASSWORD=replace-with-a-strong-password
POSTGRES_DB=callora
DATABASE_URL=postgresql://callora:URL_ENCODED_PASSWORD@db:5432/callora
PUBLIC_BASE_URL=https://calls.example.com
```

If `POSTGRES_PASSWORD` contains URL-reserved characters, percent-encode it in `DATABASE_URL`. `DATABASE_URL` must use the Compose service hostname `db`. An existing legacy `.env` works as it is: V2 keeps these names, and legacy-only entries are ignored.

If the file does not exist, the first deployment creates it with the synchronized settings, then stops and names the host settings it cannot know.

Do not add `CALLORA_IMAGE` manually. `deploy.sh` injects the exact commit-SHA image into a copy of the environment for each release.

### How settings reach the VM

The workflow sends `NAME=VALUE` lines to `deploy.sh update-secrets` over the SSH connection's standard input, never on a command line, and nothing is printed. `deploy.sh` accepts only the names it knows (see `SYNCED_SETTINGS` in the script). A name that is sent replaces the server's value, an empty value clears it, and a name that is not sent is left alone, so host-only settings can never be overwritten by the pipeline. Only names are logged.


## Required GitHub repository settings

The complete list, names only, is in [SECRETS.md](SECRETS.md). The minimum for a working deployment:

- Secrets: `IP`, `USER`, `KEY_PEM`, `DOCKER_HUB_TOKEN`, `TWILIO_ACCOUNT_SID`, `TWILIO_AUTH_TOKEN`, `CARTESIA_API_KEY`, `ELEVENLABS_API_KEY`, and `TAXI_PHONE_NUMBERS` (Secret or Variable).
- Variables: `DOCKER_HUB_USERNAME`, `ELEVENLABS_VOICE_ID`.
- Recommended: `GEMINI_API_KEY` or `OPENAI_API_KEY` (LLM understanding when the rules are unsure; Gemini wins when both are set), `TAXI_HANDOFF_NUMBER` (the human desk), `STREAM_TOKEN_SECRET`.

The deploy job still targets the `production` environment, so its protection rules and required reviewers stay in force.

In the Twilio console, each taxi number's **A call comes in** webhook is `POST https://<PUBLIC_BASE_URL host>/webhooks/twilio/voice`, and its status callback is `/webhooks/twilio/call-status`. These are the legacy paths, so numbers that were already configured need no change.


## CI/CD behavior

Pull requests run `./dev check` (rustfmt, clippy with warnings denied, and the whole test suite against a real PostgreSQL, all in containers), business configuration validation, shell syntax checks, and Compose validation. They never use deployment secrets.

A push to `main` then:

1. Builds the `linux/arm64` image. The Dockerfile cross-compiles a static binary on the runner's own architecture with cargo-zigbuild, with zig fetched from PyPI against a pinned checksum. The result is a distroless, non-root image of about 5 MB.
2. Pushes `DOCKER_HUB_USERNAME/callora:<sha>` and `:latest`, and deploys the SHA tag over SSH.
3. `deploy.sh` reclaims disk, syncs settings, pulls, starts PostgreSQL, validates Caddy, runs `callora migrate` (retried, before the backend is replaced), replaces the backend, starts Caddy, and waits for the container healthcheck (`callora healthcheck`), the internal health check and the public HTTPS health check. It rolls back automatically on failure.

Migrations create and evolve only the `callora_v2` schema, and they are forward-compatible with the previous image.

## Voice library

Most replies are pre-generated audio, and the library is built once per voice (and again, incrementally, after response texts change). It lives in the `callora_voice_library` volume. After the first V2 deployment, and whenever `businesses/*.json` responses or the voice change:

```bash
cd /opt/callora
docker compose --env-file .env -f docker-compose.prod.yml run --rm backend voice-library build --business taxi
docker compose --env-file .env -f docker-compose.prod.yml run --rm backend voice-library status
docker compose --env-file .env -f docker-compose.prod.yml restart backend
```

Only missing or changed clips are synthesized. Until the library exists, every reply uses dynamic TTS: this is slower, but works.

## Cutting over from the legacy stack

The Postgres volume, Caddy volumes, ports, `.env` host settings and webhook paths are all unchanged, so a V2 deployment replaces the legacy backend in place. Legacy data in the `public` schema is left untouched. To return to the legacy system, deploy `OLD-MAIN`: its image tags are still on Docker Hub, and V2's schema does not interfere with it.


## Disk space

Every release is pulled under its own immutable commit-SHA tag, so the VM gains a whole image per deployment. Nothing used to remove one, and a boot volume that filled up between releases failed the deployment in the worst possible place: partway through unpacking a layer, with the live configuration already replaced.

Container logs were the other half of it, and on a boot volume this size the larger half: Docker never rotates a log on its own, and the backend narrates every call turn to stdout. Every service in `docker-compose.prod.yml` now caps its log at 10 MB across 3 files, and `deploy/bootstrap-oracle-linux.sh` writes the same defaults into `/etc/docker/daemon.json` for anything started outside Compose. A container keeps its old, unbounded log file until it is next recreated, which the deployment does anyway.

Each deployment reclaims before it touches anything. Superseded `*/callora:<sha>` images, dangling images, the build cache, and containers left behind by earlier runs are removed; the incoming release, the release a rollback would restore, and anything a running container still uses are all kept. Volumes are never pruned, under any filter, because the PostgreSQL named volume is the production database.

After reclaiming, the deployment checks that `/opt/callora`, Docker's data root, and `/var/lib/containerd` each have room to unpack the release: twice the size of the release already on the host, and never less than 1 GiB. Measuring it from the image rather than fixing a number means the check tracks whatever the backend image grows into. If there is not enough, it truncates any container log over 50 MB — in place, the way `logrotate -copytruncate` does, using an image already on the host so nothing has to be pulled onto a full disk — and checks once more. Short of that it stops before the previous release is replaced, so the site keeps serving while the space is sorted out.

A refusal prints `df`, `docker system df`, **and the largest directories on the root filesystem**, because Docker's own accounting only covers Docker's files. On this VM those came to 2.3 GB of images on a disk with 28 GB used — the thing filling it was outside Docker entirely, and no amount of pruning would have found it.

Reclaiming can also be run on its own, which is the first thing to try on a host that has already filled up:

```bash
df -h /var
docker system df
bash /opt/callora/deploy.sh reclaim
```

If that is not enough, the boot volume needs to grow; images are not what is filling it.

## Recovering a lost environment file

`POSTGRES_USER`, `POSTGRES_PASSWORD`, `POSTGRES_DB`, `DATABASE_URL`, and `PUBLIC_BASE_URL` deliberately never leave the VM, so the pipeline cannot re-send them if `/opt/callora/.env` is lost. Before the atomic writes described below, a rollback on a full disk could truncate that file and take them with it.

`update-secrets` now puts them back on its own, from two places that still have them: `/opt/callora/.rollback/.env`, the backup the failed deployment took, whose lines are copied back byte for byte; and failing that, the containers still running with the values, read with `docker inspect`.

Only settings that are actually missing are written, only names are ever logged, and a value read from a container is used only if it survives an env file unquoted — anything else is reported by name rather than written back as something subtly different from what the server is running on.

## Ports 80 and 443

Caddy is the last container a deployment starts, so a port already taken by something else used to surface as a daemon error at the very end — after the backend had been replaced. The deployment now checks first, before it touches the live configuration, and names every container publishing 80 or 443 that does not belong to the `callora` Compose project, with its image and project.

It reports and refuses; it never stops anything. A container holding :80 may be the stack currently serving the site, and it may own the database volume, so what happens to it is a decision for a person and not for a deployment.

Caddy is then checked again after it starts. Compose reuses a container whose configuration has not changed, so a Caddy container created by a deployment that failed to bind the ports can be started by the next one without ever binding them — and then everything reports healthy, because Caddy's health check runs inside the container, while nothing answers from outside the VM. If the ports are not held, the deployment recreates Caddy once and checks again. A public health check that still fails says whether Caddy holds the ports, which separates a Docker problem from a VCN, NSG, firewalld, or DNS one.

## Rollback behavior

Before changing the running application, `/opt/callora/deploy.sh` saves the prior image reference and production configuration. Configuration is replaced by writing a staging file next to the destination and renaming it, so `/opt/callora/.env` is always either the old file or the new one — a full disk can no longer truncate it halfway through a rollback. If image pull, Caddy validation, migration, container startup, or any health check fails, it restores the previous backend and Caddy configuration. On a failed first deployment it stops the app containers, restores the original server environment, and preserves PostgreSQL and its volume.

Database migrations are never automatically reversed because doing so could destroy data. New migrations must therefore be backward-compatible with the previous application image. The old backend remains running while the new image is pulled and migrations execute; only the final single-container replacement creates a brief application restart.

Useful production diagnostics:

```bash
cd /opt/callora
docker compose --env-file .env -f docker-compose.prod.yml ps
docker compose --env-file .env -f docker-compose.prod.yml logs --tail=200 backend
docker compose --env-file .env -f docker-compose.prod.yml logs --tail=200 caddy
docker volume inspect callora_postgres_data
df -h /var /opt
docker system df
```

Do not run `docker compose down --volumes` in production.

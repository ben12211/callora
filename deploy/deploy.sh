#!/usr/bin/env bash
set -Eeuo pipefail
umask 077

readonly APP_DIR=/opt/callora
readonly INCOMING_DIR="$APP_DIR/incoming"
readonly COMPOSE_FILE="$APP_DIR/docker-compose.prod.yml"
readonly ENV_FILE="$APP_DIR/.env"
readonly CADDY_FILE="$APP_DIR/Caddyfile"
readonly ROLLBACK_DIR="$APP_DIR/.rollback"
readonly LAST_IMAGE_FILE="$APP_DIR/.last-successful-image"
readonly LOCK_FILE=/tmp/callora-deploy.lock
# Unpacking a release needs room for the download and the extracted layers at the same
# time. Below this, `docker pull` dies partway through writing a layer, which is how a
# deployment ends up with neither the new release nor enough room to restore the old one.
readonly REQUIRED_FREE_MIB=3072
# Container logs were never rotated, so on a small boot volume they outgrow the images by
# far. Anything past this is truncated when a deployment would otherwise be refused.
readonly LOG_TRUNCATE_ABOVE_KIB=51200
# The settings that deliberately never leave the VM, so the pipeline cannot re-send them.
readonly HOST_SETTINGS=(POSTGRES_USER POSTGRES_PASSWORD POSTGRES_DB DATABASE_URL PUBLIC_BASE_URL)

log() {
  printf '[callora-deploy] %s\n' "$*"
}

compose() {
  docker compose --env-file "$ENV_FILE" -f "$COMPOSE_FILE" "$@"
}

available_mib() {
  local path="$1"

  # df needs a path that exists; a file written later lands on its parent's filesystem,
  # so walking up answers the same question.
  while [[ ! -e "$path" && "$path" == */?* ]]; do
    path="${path%/*}"
    [[ -n "$path" ]] || path=/
  done

  df -Pk -- "$path" 2>/dev/null | awk 'NR == 2 { printf "%d\n", $4 / 1024 }'
}

storage_filesystems() {
  local docker_root
  docker_root="$(docker info --format '{{.DockerRootDir}}' 2>/dev/null || true)"
  printf '%s\n' "$APP_DIR" "${docker_root:-/var/lib/docker}"
  # containerd holds the snapshots a pull unpacks into, and that is not always the same
  # filesystem as Docker's own data root.
  if [[ -d /var/lib/containerd ]]; then
    printf '%s\n' /var/lib/containerd
  fi
}

# Prints the filesystems that are short of room, and returns non-zero if any of them is.
cramped_filesystems() {
  local path avail
  local -a cramped=()

  while IFS= read -r path; do
    avail="$(available_mib "$path")"
    [[ -n "$avail" ]] || continue
    if ((avail < REQUIRED_FREE_MIB)); then
      cramped+=("$path has ${avail} MiB free")
    fi
  done < <(storage_filesystems)

  [[ ${#cramped[@]} -eq 0 ]] && return 0
  printf '%s\n' "${cramped[*]}"
  return 1
}

# Docker offers no way to truncate the log of a container that is running, and the files
# are root-owned under its data root, so reaching them without stopping the site takes a
# throwaway container. Best effort by design: it runs only when reclaiming images was not
# enough, and only with an image already on the host, because pulling one is the very
# thing that has no room. Truncating in place is what `logrotate -copytruncate` does; the
# daemon keeps appending to the same file afterwards.
truncate_oversized_container_logs() {
  local docker_root helper='' candidate

  docker_root="$(docker info --format '{{.DockerRootDir}}' 2>/dev/null || true)"
  [[ -n "$docker_root" ]] || docker_root=/var/lib/docker

  for candidate in caddy:2-alpine postgres:16-alpine alpine:latest busybox:latest; do
    if docker image inspect "$candidate" >/dev/null 2>&1; then
      helper="$candidate"
      break
    fi
  done
  [[ -n "$helper" ]] || {
    log 'No image is available locally to reclaim container logs with; skipping.'
    return 0
  }

  log "Truncating container logs larger than $((LOG_TRUNCATE_ABOVE_KIB / 1024)) MiB."
  # label=disable because relabelling Docker's own data root would be worse than the
  # problem; this container is removed the moment the find returns.
  docker run --rm --network none --user 0:0 --security-opt label=disable \
    --volume "$docker_root/containers:/containers" \
    --entrypoint sh "$helper" -c \
    "find /containers -name '*-json.log' -size +${LOG_TRUNCATE_ABOVE_KIB}k -exec truncate -s 0 {} ';'" \
    >/dev/null 2>&1 || log 'Container logs could not be reclaimed; continuing.'
}

ensure_disk_space() {
  local shortfall

  shortfall="$(cramped_filesystems)" && return 0
  log "Still short of room after reclaiming images: $shortfall."
  truncate_oversized_container_logs
  shortfall="$(cramped_filesystems)" && return 0

  log "Not enough disk space to unpack a release: $shortfall; ${REQUIRED_FREE_MIB} MiB are needed."
  log 'Grow the boot volume, or free space on the VM, then re-run the deployment.'
  # The run that refuses to deploy is also the run that has to say what is holding the
  # disk, or the next step is somebody guessing over SSH.
  report_disk_usage
  return 1
}

report_disk_usage() {
  local path
  while IFS= read -r path; do
    log "$(df -Ph -- "$path" 2>/dev/null | awk 'NR == 2 { printf "%s: %s of %s used, %s free", "'"$path"'", $3, $2, $4 }')"
  done < <(storage_filesystems)
  docker system df 2>/dev/null || true
}

# Every release is pulled under its own immutable commit-SHA tag, so the host gained a
# whole image per deployment and nothing ever removed one. Reclaiming is part of
# deploying: a disk that fills between releases takes the site down at the worst moment,
# during the pull, with the live configuration already replaced.
reclaim_disk_space() {
  local keep_image="${1:-}"
  local last_image='' image

  if [[ -f "$LAST_IMAGE_FILE" ]]; then
    IFS= read -r last_image < "$LAST_IMAGE_FILE" || true
  fi

  # Only containers left behind by earlier deployments; anything from this one is younger.
  docker container prune --force --filter until=24h >/dev/null 2>&1 || true

  while IFS= read -r image; do
    [[ -n "$image" && "$image" != *'<none>'* ]] || continue
    # The incoming release and the one a rollback would restore both stay.
    [[ "$image" != "$keep_image" && "$image" != "$last_image" ]] || continue
    # The daemon refuses to remove an image a container still uses, which is the second
    # guard on whatever is serving traffic right now.
    docker image rm -- "$image" >/dev/null 2>&1 || true
  done < <(docker image ls --format '{{.Repository}}:{{.Tag}}' --filter 'reference=*/callora:*' 2>/dev/null)

  docker image prune --force >/dev/null 2>&1 || true
  docker builder prune --force >/dev/null 2>&1 || true
  # Never prune volumes here, under any filter: the PostgreSQL named volume is the
  # production database.
}

setting_present() {
  local name="$1" file="$2"
  [[ -f "$file" ]] && grep -Eq "^[[:space:]]*$name[[:space:]]*=[[:space:]]*[^[:space:]]" "$file"
}

# Reads a value out of a container the stack is still running. `docker compose` cannot be
# used here: it interpolates the very file that is missing the values, and fails first.
container_setting() {
  local service="$1" name="$2" id

  id="$(docker ps --all --quiet \
    --filter 'label=com.docker.compose.project=callora' \
    --filter "label=com.docker.compose.service=$service" 2>/dev/null | head -n 1)"
  [[ -n "$id" ]] || return 0

  docker inspect --format '{{range .Config.Env}}{{println .}}{{end}}' "$id" 2>/dev/null |
    sed -n "s/^$name=//p" | head -n 1
}

# The host-only settings are recoverable from the backup a failed deployment left behind,
# and from the containers still running with them - which is the whole reason a lost .env
# does not have to mean a hand-typed database password. Copying the backup's line verbatim
# keeps a value with spaces, quoting or a `$` in it byte-for-byte what it was.
recover_missing_settings() {
  local setting value service
  local -a recovered=() unrecovered=()

  for setting in "$@"; do
    setting_present "$setting" "$ENV_FILE" && continue

    value=''
    if [[ -f "$ROLLBACK_DIR/.env" ]]; then
      value="$(sed -n "s/^[[:space:]]*$setting[[:space:]]*=/$setting=/p" "$ROLLBACK_DIR/.env" | head -n 1)"
    fi

    if [[ -z "$value" ]]; then
      case "$setting" in
        POSTGRES_USER|POSTGRES_PASSWORD|POSTGRES_DB) service=db ;;
        DATABASE_URL|SECRETS_KEY) service=backend ;;
        PUBLIC_BASE_URL) service=caddy ;;
        *) service='' ;;
      esac
      if [[ -n "$service" ]]; then
        value="$(container_setting "$service" "$setting")"
        # A resolved value is written back unquoted, so only take one that survives the
        # round trip through an env file untouched. Anything else is reported instead of
        # being written as something subtly different from what the server is using.
        if [[ -n "$value" && "$value" =~ ^[^[:space:]\$\#\\\"\']+$ ]]; then
          value="$setting=$value"
        else
          value=''
        fi
      fi
    fi

    if [[ -n "$value" ]]; then
      printf '%s\n' "$value" >> "$ENV_FILE"
      recovered+=("$setting")
    else
      unrecovered+=("$setting")
    fi
  done

  [[ ${#recovered[@]} -eq 0 ]] || \
    log "Recovered from the previous release, by name only: ${recovered[*]}."
  [[ ${#unrecovered[@]} -eq 0 ]] || \
    log "Not recoverable from this server: ${unrecovered[*]}."
}

# install(1) writes straight into the destination, so a failure partway through - a full
# disk above all - leaves the live file truncated. That is how a failed rollback erased
# the production .env. Staging beside the destination and renaming means the file on disk
# is either the old one or the new one, never half of either.
install_atomic() {
  local mode="$1" source="$2" target="$3"
  local staged

  staged="$(mktemp "$target.XXXXXX")" || return 1
  if ! install -m "$mode" -- "$source" "$staged"; then
    rm -f -- "$staged"
    return 1
  fi
  mv -f -- "$staged" "$target"
}

wait_for_healthy() {
  local service="$1"
  local attempts="${2:-60}"
  local container_id status

  for ((attempt = 1; attempt <= attempts; attempt++)); do
    container_id="$(compose ps -q "$service")"
    if [[ -n "$container_id" ]]; then
      status="$(docker inspect --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' "$container_id")"
      if [[ "$status" == healthy || "$status" == running ]]; then
        return 0
      fi
      if [[ "$status" == exited || "$status" == dead ]]; then
        compose logs --tail=100 "$service" || true
        return 1
      fi
    fi
    sleep 2
  done

  compose logs --tail=100 "$service" || true
  return 1
}

wait_for_public_health() {
  local attempts="${1:-30}"
  local caddy_id public_base_url

  caddy_id="$(compose ps -q caddy)"
  [[ -n "$caddy_id" ]] || {
    log 'Caddy is not running; cannot check the public health endpoint.'
    return 1
  }

  public_base_url="$(
    docker inspect --format '{{range .Config.Env}}{{println .}}{{end}}' "$caddy_id" |
      sed -n 's/^PUBLIC_BASE_URL=//p' |
      head -n 1
  )"
  [[ "$public_base_url" == https://* ]] || {
    log 'Caddy does not have a valid HTTPS PUBLIC_BASE_URL.'
    return 1
  }

  for ((attempt = 1; attempt <= attempts; attempt++)); do
    if curl --fail --silent --show-error --max-time 5 -- "${public_base_url%/}/health" >/dev/null; then
      return 0
    fi
    sleep 5
  done

  log 'The public health endpoint did not become ready.'
  return 1
}

prepare_incoming_env() {
  local new_image="$1"

  [[ "$new_image" =~ ^[a-z0-9]+([._-][a-z0-9]+)*/callora:[0-9a-f]{40}$ ]] || {
    log 'The image must be a Docker Hub Callora image with an immutable commit-SHA tag.'
    return 1
  }

  [[ -f "$ENV_FILE" ]] || {
    log "$ENV_FILE is missing; create the production application environment before deploying."
    return 1
  }

  awk '!/^[[:space:]]*CALLORA_IMAGE[[:space:]]*=/' "$ENV_FILE" > "$INCOMING_DIR/callora.env"
  printf 'CALLORA_IMAGE="%s"\n' "$new_image" >> "$INCOMING_DIR/callora.env"
  chmod 0600 "$INCOMING_DIR/callora.env"
}

validate_incoming() {
  local new_image="$1"
  local next_compose=(docker compose
    --env-file "$INCOMING_DIR/callora.env"
    -f "$INCOMING_DIR/docker-compose.prod.yml")

  for file in callora.env docker-compose.prod.yml Caddyfile deploy.sh; do
    [[ -f "$INCOMING_DIR/$file" ]] || {
      log "Missing deployment file: $file"
      return 1
    }
  done

  # Each check reports what failed: a bare `grep -q` under `set -e` aborts the whole
  # deployment with no output at all, which is unusable to debug from CI logs.
  "${next_compose[@]}" config -q || {
    log 'The incoming Compose configuration failed validation.'
    return 1
  }

  local images
  images="$("${next_compose[@]}" config --images)" || {
    log 'Could not resolve the images from the incoming Compose configuration.'
    return 1
  }
  grep -Fqx "$new_image" <<<"$images" || {
    log "The incoming configuration does not reference $new_image."
    log "Resolved images: $(tr '\n' ' ' <<<"$images")"
    return 1
  }
}

backup_current() {
  rm -rf -- "$ROLLBACK_DIR"
  install -d -m 0700 "$ROLLBACK_DIR"

  if [[ -f "$ENV_FILE" && -f "$COMPOSE_FILE" && -f "$CADDY_FILE" ]]; then
    install -m 0600 "$ENV_FILE" "$ROLLBACK_DIR/.env"
    install -m 0644 "$COMPOSE_FILE" "$ROLLBACK_DIR/docker-compose.prod.yml"
    install -m 0644 "$CADDY_FILE" "$ROLLBACK_DIR/Caddyfile"
    : > "$ROLLBACK_DIR/previous-release"
  elif [[ -f "$ENV_FILE" && ! -e "$COMPOSE_FILE" && ! -e "$CADDY_FILE" ]]; then
    install -m 0600 "$ENV_FILE" "$ROLLBACK_DIR/.env"
    : > "$ROLLBACK_DIR/first-deploy"
  elif [[ -e "$ENV_FILE" || -e "$COMPOSE_FILE" || -e "$CADDY_FILE" ]]; then
    log 'Production configuration is incomplete; refusing to overwrite it.'
    return 1
  else
    log "$ENV_FILE is missing; refusing to deploy without application configuration."
    return 1
  fi
}

install_incoming() {
  install_atomic 0600 "$INCOMING_DIR/callora.env" "$ENV_FILE"
  install_atomic 0644 "$INCOMING_DIR/docker-compose.prod.yml" "$COMPOSE_FILE"
  install_atomic 0644 "$INCOMING_DIR/Caddyfile" "$CADDY_FILE"
  install_atomic 0755 "$INCOMING_DIR/deploy.sh" "$APP_DIR/deploy.sh"
}

perform_rollback() {
  trap - ERR
  log 'Rolling back application configuration and backend image.'

  # A deployment that failed because the disk was full leaves a rollback needing a few
  # kilobytes of that same disk to write the previous configuration back. Reclaim first,
  # or the recovery path fails for the exact reason the deployment did.
  reclaim_disk_space

  if [[ -f "$ROLLBACK_DIR/previous-release" ]]; then
    install_atomic 0600 "$ROLLBACK_DIR/.env" "$ENV_FILE" || return 1
    install_atomic 0644 "$ROLLBACK_DIR/docker-compose.prod.yml" "$COMPOSE_FILE" || return 1
    install_atomic 0644 "$ROLLBACK_DIR/Caddyfile" "$CADDY_FILE" || return 1
    compose up -d db || return 1
    wait_for_healthy db 60 || return 1
    compose up -d --no-deps backend || return 1
    wait_for_healthy backend 60 || return 1
    compose up -d --no-deps caddy || return 1
    wait_for_healthy caddy 60 || return 1
    log 'Previous application release restored. Database migrations were not reversed.'
    return 0
  fi

  if [[ -f "$ROLLBACK_DIR/first-deploy" && -f "$ENV_FILE" && -f "$COMPOSE_FILE" ]]; then
    compose stop backend caddy 2>/dev/null || true
    install_atomic 0600 "$ROLLBACK_DIR/.env" "$ENV_FILE" || return 1
    log 'First deployment stopped; PostgreSQL and its named volume were preserved.'
    return 0
  fi

  log 'No rollback release is available.'
  return 1
}

deploy_release() {
  local new_image="$1"
  local migrated=false

  prepare_incoming_env "$new_image"
  validate_incoming "$new_image"

  # Both of these run before anything live is replaced, so a host without room for this
  # release simply keeps serving the previous one, instead of failing mid-pull with its
  # configuration already swapped.
  log 'Reclaiming disk space held by superseded releases.'
  reclaim_disk_space "$new_image"
  ensure_disk_space || {
    log 'Refusing to deploy; the running release has not been touched.'
    return 1
  }

  backup_current
  install_incoming

  on_error() {
    local exit_code=$?
    local line="$1"
    log "Deployment failed near line $line."
    perform_rollback || log 'Automatic rollback could not restore a previous release.'
    exit "$exit_code"
  }
  trap 'on_error $LINENO' ERR

  log 'Pulling immutable backend and supporting images.'
  compose pull backend db caddy

  log 'Starting PostgreSQL without replacing its named volume.'
  compose up -d db
  wait_for_healthy db 60

  log 'Validating the Caddy configuration.'
  compose run --rm --no-deps caddy caddy validate --config /etc/caddy/Caddyfile --adapter caddyfile

  log 'Running database migrations under the application migration lock.'
  for attempt in {1..15}; do
    if compose run --rm --no-deps backend node dist/db/migrate.js; then
      migrated=true
      break
    fi
    log "Migration attempt $attempt failed; retrying."
    sleep 2
  done
  [[ "$migrated" == true ]]
  compose run --rm --no-deps backend node dist/db/seed.js

  log 'Replacing the backend only after migrations succeed.'
  compose up -d --no-deps backend
  wait_for_healthy backend 60

  log 'Starting or refreshing the HTTPS reverse proxy.'
  compose up -d --no-deps caddy
  wait_for_healthy caddy 60

  compose exec -T backend node -e \
    "fetch('http://127.0.0.1:3000/health').then(r=>{if(!r.ok)process.exit(1)}).catch(()=>process.exit(1))"

  log 'Checking the public HTTPS health endpoint.'
  wait_for_public_health 30

  trap - ERR
  log 'Database, backend, Caddy, internal, and public health checks passed.'
}

confirm_release() {
  [[ -d "$ROLLBACK_DIR" ]] || {
    log 'No pending release is awaiting confirmation.'
    return 1
  }

  compose config --images |
    grep -E '^[a-z0-9]+([._-][a-z0-9]+)*/callora:[0-9a-f]{40}$' |
    head -n 1 > "$LAST_IMAGE_FILE"
  chmod 0600 "$LAST_IMAGE_FILE"
  rm -rf -- "$ROLLBACK_DIR"
  log 'Deployment confirmed.'
}

rollback_release() {
  [[ -d "$ROLLBACK_DIR" ]] || {
    log 'No pending rollback is available.'
    return 1
  }
  perform_rollback
}

update_runtime_secrets() {
  local twilio_account_sid twilio_auth_token openai_api_key allow_list temp_env
  local voice_provider elevenlabs_api_key elevenlabs_agent_id
  local cartesia_api_key cartesia_voice_id
  local deepdub_api_key deepdub_voice_id renikud_url
  local admin_email admin_password admin_api_key secrets_key

  IFS= read -r twilio_account_sid
  IFS= read -r twilio_auth_token
  IFS= read -r openai_api_key
  # Optional, and absent when an older workflow sends only three lines.
  allow_list=''
  IFS= read -r allow_list || true
  # Optional too: an older workflow sends nothing for the provider, which means openai.
  voice_provider=''
  elevenlabs_api_key=''
  elevenlabs_agent_id=''
  cartesia_api_key=''
  cartesia_voice_id=''
  IFS= read -r voice_provider || true
  IFS= read -r elevenlabs_api_key || true
  IFS= read -r elevenlabs_agent_id || true
  IFS= read -r cartesia_api_key || true
  IFS= read -r cartesia_voice_id || true
  # Control-plane credentials, optional so an older workflow that sends nothing here
  # still deploys; the dashboard then keeps whatever administrator already exists.
  admin_email=''
  admin_password=''
  admin_api_key=''
  IFS= read -r admin_email || true
  IFS= read -r admin_password || true
  IFS= read -r admin_api_key || true
  # Encrypts the credentials entered in the dashboard. Optional: an older workflow sends
  # nothing, which must leave whatever key the server already has untouched, because
  # replacing it would make every stored credential unreadable.
  secrets_key=''
  IFS= read -r secrets_key || true
  # Deepdub, appended after everything above for the same reason every block before it was:
  # a workflow that does not send these lines leaves them empty instead of shifting the
  # meaning of the lines it does send. An empty RENIKUD_URL simply leaves the optional
  # pronunciation sidecar switched off.
  deepdub_api_key=''
  deepdub_voice_id=''
  renikud_url=''
  IFS= read -r deepdub_api_key || true
  IFS= read -r deepdub_voice_id || true
  IFS= read -r renikud_url || true
  [[ -n "$voice_provider" ]] || voice_provider=openai

  [[ "$twilio_account_sid" =~ ^AC[0-9a-fA-F]{32}$ ]] || {
    log 'TWILIO_ACCOUNT_SID is not a valid Twilio Account SID.'
    return 1
  }
  [[ "$twilio_auth_token" =~ ^[0-9a-fA-F]{32}$ ]] || {
    log 'TWILIO_AUTH_TOKEN is not a valid Twilio Auth Token.'
    return 1
  }
  case "$voice_provider" in
    openai|elevenlabs|cartesia|deepdub) ;;
    *)
      log 'VOICE_PROVIDER must be one of openai, elevenlabs, cartesia, or deepdub.'
      return 1
      ;;
  esac

  # A credential may arrive here or be stored in the dashboard, which keeps it encrypted
  # in the database. A malformed one still aborts; a missing one is only reported, since
  # the deployment may be relying on what an operator already entered in the browser.
  check_secret() {
    local name="$1" value="$2"
    if [[ "$value" == *$'\r'* ]]; then
      log "$name must be a single-line value."
      return 1
    fi
    [[ -n "$value" ]] || missing_secrets+=("$name")
  }

  local -a missing_secrets=()
  case "$voice_provider" in
    openai)
      check_secret OPENAI_API_KEY "$openai_api_key" || return 1
      ;;
    elevenlabs)
      check_secret ELEVENLABS_API_KEY "$elevenlabs_api_key" || return 1
      check_secret ELEVENLABS_AGENT_ID "$elevenlabs_agent_id" || return 1
      ;;
    cartesia)
      check_secret CARTESIA_API_KEY "$cartesia_api_key" || return 1
      check_secret CARTESIA_VOICE_ID "$cartesia_voice_id" || return 1
      # Cartesia covers speech only; the reasoning turn runs on the OpenAI text model.
      check_secret OPENAI_API_KEY "$openai_api_key" || return 1
      ;;
    deepdub)
      check_secret DEEPDUB_API_KEY "$deepdub_api_key" || return 1
      check_secret DEEPDUB_VOICE_ID "$deepdub_voice_id" || return 1
      # Deepdub speaks; Cartesia supplies streaming Hebrew STT and OpenAI the reasoning turn.
      check_secret CARTESIA_API_KEY "$cartesia_api_key" || return 1
      check_secret OPENAI_API_KEY "$openai_api_key" || return 1
      ;;
  esac
  if [[ ${#missing_secrets[@]} -gt 0 ]]; then
    log "Not supplied by the pipeline: ${missing_secrets[*]}. They must be stored in the dashboard under Providers, or calls will answer with the static greeting."
  fi
  [[ -z "$secrets_key" || ${#secrets_key} -ge 16 ]] || {
    log 'SECRETS_KEY must be at least 16 characters.'
    return 1
  }
  # Both halves of the bootstrap administrator are needed, or neither.
  if [[ -n "$admin_email" || -n "$admin_password" ]]; then
    [[ "$admin_email" =~ ^[^[:space:]@]+@[^[:space:]@]+\.[^[:space:]@]+$ ]] || {
      log 'ADMIN_EMAIL must be a single email address when ADMIN_PASSWORD is set.'
      return 1
    }
    [[ ${#admin_password} -ge 12 ]] || {
      log 'ADMIN_PASSWORD must be at least 12 characters.'
      return 1
    }
  fi
  [[ -z "$admin_api_key" || ${#admin_api_key} -ge 16 ]] || {
    log 'ADMIN_API_KEY must be at least 16 characters when set.'
    return 1
  }
  # Empty means "no allowlist"; anything else must be E.164 numbers separated by commas.
  [[ -z "$allow_list" || "$allow_list" =~ ^[[:space:]]*\+[1-9][0-9]{7,14}([[:space:]]*,[[:space:]]*\+[1-9][0-9]{7,14})*[[:space:]]*$ ]] || {
    log 'ALLOW_LIST must be empty or a comma-separated list of E.164 numbers.'
    return 1
  }
  # The secrets handed to this function are enough to seed the file from nothing, so a
  # first deployment onto a freshly bootstrapped VM is not a dead end. What it cannot
  # invent are the host-specific settings that deliberately never leave the VM, so those
  # are reported by name once the file is written instead of being refused up front.
  local baseline="$ENV_FILE"
  if [[ ! -f "$ENV_FILE" ]]; then
    log "$ENV_FILE does not exist yet; creating it from the credentials the pipeline supplied."
    baseline=/dev/null
  fi

  temp_env="$(mktemp "$APP_DIR/.env.XXXXXX")"
  trap 'rm -f -- "$temp_env"' RETURN
  # SECRETS_KEY is dropped only when a new one was sent: an empty value means "keep the
  # key this server already has", never "erase it".
  awk -v replace_secrets_key="${secrets_key:+1}" '
    (replace_secrets_key == "" || !/^[[:space:]]*SECRETS_KEY[[:space:]]*=/) &&
    !/^[[:space:]]*TWILIO_ACCOUNT_SID[[:space:]]*=/ &&
    !/^[[:space:]]*TWILIO_AUTH_TOKEN[[:space:]]*=/ &&
    !/^[[:space:]]*OPENAI_API_KEY[[:space:]]*=/ &&
    !/^[[:space:]]*ALLOW_LIST[[:space:]]*=/ &&
    !/^[[:space:]]*VOICE_PROVIDER[[:space:]]*=/ &&
    !/^[[:space:]]*ELEVENLABS_API_KEY[[:space:]]*=/ &&
    !/^[[:space:]]*ELEVENLABS_AGENT_ID[[:space:]]*=/ &&
    !/^[[:space:]]*CARTESIA_API_KEY[[:space:]]*=/ &&
    !/^[[:space:]]*CARTESIA_VOICE_ID[[:space:]]*=/ &&
    !/^[[:space:]]*DEEPDUB_API_KEY[[:space:]]*=/ &&
    !/^[[:space:]]*DEEPDUB_VOICE_ID[[:space:]]*=/ &&
    !/^[[:space:]]*RENIKUD_URL[[:space:]]*=/ &&
    !/^[[:space:]]*ADMIN_EMAIL[[:space:]]*=/ &&
    !/^[[:space:]]*ADMIN_PASSWORD[[:space:]]*=/ &&
    !/^[[:space:]]*ADMIN_API_KEY[[:space:]]*=/
  ' "$baseline" > "$temp_env"
  printf 'TWILIO_ACCOUNT_SID=%s\n' "$twilio_account_sid" >> "$temp_env"
  printf 'TWILIO_AUTH_TOKEN=%s\n' "$twilio_auth_token" >> "$temp_env"
  printf 'OPENAI_API_KEY=%s\n' "$openai_api_key" >> "$temp_env"
  printf 'ALLOW_LIST=%s\n' "$allow_list" >> "$temp_env"
  printf 'VOICE_PROVIDER=%s\n' "$voice_provider" >> "$temp_env"
  printf 'ELEVENLABS_API_KEY=%s\n' "$elevenlabs_api_key" >> "$temp_env"
  printf 'ELEVENLABS_AGENT_ID=%s\n' "$elevenlabs_agent_id" >> "$temp_env"
  printf 'CARTESIA_API_KEY=%s\n' "$cartesia_api_key" >> "$temp_env"
  printf 'CARTESIA_VOICE_ID=%s\n' "$cartesia_voice_id" >> "$temp_env"
  printf 'DEEPDUB_API_KEY=%s\n' "$deepdub_api_key" >> "$temp_env"
  printf 'DEEPDUB_VOICE_ID=%s\n' "$deepdub_voice_id" >> "$temp_env"
  printf 'RENIKUD_URL=%s\n' "$renikud_url" >> "$temp_env"
  printf 'ADMIN_EMAIL=%s\n' "$admin_email" >> "$temp_env"
  printf 'ADMIN_PASSWORD=%s\n' "$admin_password" >> "$temp_env"
  printf 'ADMIN_API_KEY=%s\n' "$admin_api_key" >> "$temp_env"
  if [[ -n "$secrets_key" ]]; then
    printf 'SECRETS_KEY=%s\n' "$secrets_key" >> "$temp_env"
  fi
  chmod 0600 "$temp_env"
  mv -f -- "$temp_env" "$ENV_FILE"
  trap - RETURN

  # A deployment that ran out of disk used to leave this file truncated, taking the
  # host-only settings with it. They are put back from the server itself rather than
  # asking somebody to retype a database password that PostgreSQL's volume still expects.
  # An empty SECRETS_KEY from the pipeline means "keep this server's key", which after a
  # truncation means recovering it too, or every stored credential becomes unreadable.
  local -a recoverable=("${HOST_SETTINGS[@]}")
  [[ -n "$secrets_key" ]] || recoverable+=(SECRETS_KEY)
  recover_missing_settings "${recoverable[@]}"

  # Compose treats these as mandatory, and none of them can come from the pipeline: the
  # database credentials and the public URL are host-specific by design. Naming the ones
  # that are absent here turns an opaque "variable is not set" from `docker compose` at
  # deploy time into one actionable message, with the rest of the file already written.
  local -a missing_host_settings=()
  local setting
  for setting in "${HOST_SETTINGS[@]}"; do
    setting_present "$setting" "$ENV_FILE" || missing_host_settings+=("$setting")
  done
  if [[ ${#missing_host_settings[@]} -gt 0 ]]; then
    log "$ENV_FILE is missing the host-specific settings: ${missing_host_settings[*]}."
    log "Add them on the VM (see 'Production application environment on the VM' in DEPLOYMENT.md), then re-run this deployment. The credentials from the pipeline have already been written."
    return 1
  fi

  if [[ -n "$allow_list" ]]; then
    log "Runtime credentials updated for the $voice_provider voice provider; caller allowlist is active."
  else
    log "Runtime credentials updated for the $voice_provider voice provider; no caller allowlist."
  fi
}

main() {
  [[ -d "$APP_DIR" ]] || {
    log "$APP_DIR does not exist; run the bootstrap script first."
    exit 1
  }

  exec 9>"$LOCK_FILE"
  flock -n 9 || {
    log 'Another deployment is already running.'
    exit 1
  }

  # Baseline reporting so no command can abort the script without saying where.
  # deploy_release installs its own ERR trap once a rollback becomes possible.
  trap 'log "deploy.sh failed at line $LINENO with exit $?."' ERR

  case "${1:-}" in
    deploy)
      [[ $# -eq 2 ]] || { log 'Usage: deploy.sh deploy IMAGE'; exit 2; }
      deploy_release "$2"
      ;;
    confirm)
      [[ $# -eq 1 ]] || { log 'Usage: deploy.sh confirm'; exit 2; }
      confirm_release
      ;;
    rollback)
      [[ $# -eq 1 ]] || { log 'Usage: deploy.sh rollback'; exit 2; }
      rollback_release
      ;;
    update-secrets)
      [[ $# -eq 1 ]] || { log 'Usage: deploy.sh update-secrets'; exit 2; }
      update_runtime_secrets
      ;;
    reclaim)
      # Callable on its own so a host that has already filled up can be made writable
      # again before the deployment starts writing to it.
      [[ $# -eq 1 ]] || { log 'Usage: deploy.sh reclaim'; exit 2; }
      reclaim_disk_space
      log "Disk space reclaimed; $(available_mib "$APP_DIR") MiB free on $APP_DIR."
      # Printed every deployment: when the disk fills again, the run that failed is also
      # the run that says what was holding the space.
      report_disk_usage
      ;;
    *)
      log 'Usage: deploy.sh {deploy IMAGE|confirm|rollback|update-secrets|reclaim}'
      exit 2
      ;;
  esac
}

main "$@"

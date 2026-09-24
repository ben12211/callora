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
# Unpacking a release needs room for the compressed download and the extracted layers at
# the same time; short of that, `docker pull` dies partway through writing a layer, which
# is how a deployment ends up with neither the new release nor room to restore the old
# one. How much that is depends on the image, so it is measured from the release already
# on the host rather than fixed at a number someone guessed.
readonly FREE_MIB_PER_IMAGE=2
readonly MIN_FREE_MIB=1024
# Container logs were never rotated, so on a small boot volume they outgrow the images by
# far. Anything past this is truncated when a deployment would otherwise be refused.
readonly LOG_TRUNCATE_ABOVE_KIB=51200
# The settings that deliberately never leave the VM, so the pipeline cannot re-send them.
readonly HOST_SETTINGS=(POSTGRES_USER POSTGRES_PASSWORD POSTGRES_DB DATABASE_URL PUBLIC_BASE_URL)
# `name:` in the Compose file. Containers carrying this project label are this release's.
readonly COMPOSE_PROJECT=callora
# The ports Caddy takes on the host, and the last thing a deployment does. A conflict here
# is found after the backend has already been replaced, so it is checked at the start.
readonly PUBLISHED_PORTS=(80 443)

log() {
  printf '[callora-deploy] %s\n' "$*"
}

required_free_mib() {
  local last_image='' bytes='' required

  if [[ -f "$LAST_IMAGE_FILE" ]]; then
    IFS= read -r last_image < "$LAST_IMAGE_FILE" || true
  fi
  if [[ -n "$last_image" ]]; then
    bytes="$(docker image inspect --format '{{.Size}}' "$last_image" 2>/dev/null || true)"
  fi
  [[ "$bytes" =~ ^[0-9]+$ ]] || bytes=0

  required=$((bytes / 1048576 * FREE_MIB_PER_IMAGE))
  ((required >= MIN_FREE_MIB)) || required=$MIN_FREE_MIB
  printf '%s\n' "$required"
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
  local required="$1"
  local path avail
  local -a cramped=()

  while IFS= read -r path; do
    avail="$(available_mib "$path")"
    [[ -n "$avail" ]] || continue
    if ((avail < required)); then
      cramped+=("$path has ${avail} MiB free")
    fi
  done < <(storage_filesystems)

  [[ ${#cramped[@]} -eq 0 ]] && return 0
  printf '%s\n' "${cramped[*]}"
  return 1
}

# The maintenance below needs a shell with root's view of the host, and pulling an image
# is precisely what a disk with no room cannot do. So it runs in whatever is already here,
# or it does not run.
local_helper_image() {
  local candidate
  for candidate in caddy:2-alpine postgres:16-alpine alpine:latest busybox:latest; do
    if docker image inspect "$candidate" >/dev/null 2>&1; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done
  return 1
}

# `docker system df` only ever accounts for Docker's own files. When those come to a small
# fraction of a full disk - which is exactly what happened here, 2.3 GB of images on a
# disk with 28 GB used - the answer is somewhere outside Docker, and finding it should not
# require anyone to SSH in. Mounted read-only; this only measures.
report_largest_directories() {
  local helper line

  helper="$(local_helper_image)" || return 0

  log 'Largest directories on the root filesystem (MiB):'
  while IFS= read -r line; do
    if [[ -n "$line" ]]; then
      log "  $line"
    fi
  done < <(docker run --rm --network none --user 0:0 --security-opt label=disable \
    --volume /:/host:ro --entrypoint sh "$helper" -c \
    'cd /host && du -x -m -d 3 . 2>/dev/null | sort -n | tail -20' 2>/dev/null || true)
}

# Docker offers no way to truncate the log of a container that is running, and the files
# are root-owned under its data root, so reaching them without stopping the site takes a
# throwaway container. Best effort by design: it runs only when reclaiming images was not
# enough. Truncating in place is what `logrotate -copytruncate` does; the daemon keeps
# appending to the same file afterwards.
truncate_oversized_container_logs() {
  local docker_root helper

  docker_root="$(docker info --format '{{.DockerRootDir}}' 2>/dev/null || true)"
  [[ -n "$docker_root" ]] || docker_root=/var/lib/docker

  helper="$(local_helper_image)" || {
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
  local required shortfall

  required="$(required_free_mib)"
  shortfall="$(cramped_filesystems "$required")" && return 0
  log "Still short of the ${required} MiB a release needs: $shortfall."
  truncate_oversized_container_logs
  shortfall="$(cramped_filesystems "$required")" && return 0

  log "Not enough disk space to unpack a release: $shortfall; ${required} MiB are needed."
  log 'Grow the boot volume, or free space on the VM, then re-run the deployment.'
  # The run that refuses to deploy is also the run that has to say what is holding the
  # disk, or the next step is somebody guessing over SSH.
  report_disk_usage
  report_largest_directories
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

# Reported as-is, never acted on: a container publishing :80 may well be the stack that
# is currently serving the site, and deciding what happens to it is not a deployment's
# call to make.
conflicting_port_containers() {
  local port id project name image
  local -a conflicts=()

  for port in "${PUBLISHED_PORTS[@]}"; do
    while IFS= read -r id; do
      [[ -n "$id" ]] || continue
      project="$(docker inspect --format '{{index .Config.Labels "com.docker.compose.project"}}' "$id" 2>/dev/null || true)"
      # Our own containers are replaced by Compose in the ordinary way.
      [[ "$project" != "$COMPOSE_PROJECT" ]] || continue
      name="$(docker inspect --format '{{.Name}}' "$id" 2>/dev/null || true)"
      image="$(docker inspect --format '{{.Config.Image}}' "$id" 2>/dev/null || true)"
      conflicts+=("${name#/} on :$port (${image:-unknown image}${project:+, Compose project $project})")
    done < <(docker ps --quiet --filter "publish=$port" 2>/dev/null)
  done

  [[ ${#conflicts[@]} -eq 0 ]] && return 0
  printf '%s\n' "${conflicts[@]}"
  return 1
}

ensure_ports_available() {
  local conflicts line

  conflicts="$(conflicting_port_containers)" && return 0

  log 'Another stack on this VM already publishes the ports this release needs:'
  while IFS= read -r line; do
    if [[ -n "$line" ]]; then
      log "  $line"
    fi
  done <<<"$conflicts"
  log 'Callora cannot bind ports 80 and 443 while those containers hold them, and stopping'
  log 'something a deployment does not own - it may be serving the site, and it may own the'
  log 'database - is a decision for a person. Resolve it on the VM, then deploy again.'
  report_containers
  return 1
}

# Printed whenever a deployment fails, because most of what goes wrong on a host that has
# been deployed to many times is explained by what is already running on it.
report_containers() {
  local line

  log 'Containers on this host:'
  while IFS= read -r line; do
    if [[ -n "$line" ]]; then
      log "  $line"
    fi
  done < <(docker ps --all \
    --format '{{.Names}} | {{.Image}} | {{.Status}} | {{.Ports}} | {{.Label "com.docker.compose.project"}}' \
    2>/dev/null || true)
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
    log "Restored from this server (names only, never values): ${recovered[*]}."

  # Only the settings the stack cannot start without are worth reporting. A SECRETS_KEY
  # that is absent everywhere means this server simply never had one, which is a
  # supported way to run and not a failed recovery.
  local -a missing_required=()
  for setting in "${unrecovered[@]}"; do
    case " ${HOST_SETTINGS[*]} " in
      *" $setting "*) missing_required+=("$setting") ;;
    esac
  done
  [[ ${#missing_required[@]} -eq 0 ]] || \
    log "Not recoverable from this server: ${missing_required[*]}."
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

# Compose reuses a container whose configuration has not changed, which means Caddy can
# come up as an existing container that never bound the host ports - the last deployment
# failed to bind them, and this one only started it again. Everything then reports
# healthy, because Caddy's own health check runs inside the container, and nothing at all
# answers from outside. Nowhere else does "running" differ this far from "serving".
caddy_publishes_ports() {
  local id published port

  id="$(compose ps -q caddy 2>/dev/null)" || return 1
  [[ -n "$id" ]] || return 1

  published="$(docker inspect --format \
    '{{range $port, $binding := .NetworkSettings.Ports}}{{if $binding}}{{$port}} {{end}}{{end}}' \
    "$id" 2>/dev/null || true)"

  for port in "${PUBLISHED_PORTS[@]}"; do
    [[ "$published" == *"$port/tcp"* ]] || return 1
  done
}

wait_for_public_health() {
  local attempts="${1:-30}"
  local caddy_id public_base_url last_error=''

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

  # Every attempt printing the same refusal buries the one line that explains it.
  for ((attempt = 1; attempt <= attempts; attempt++)); do
    last_error="$(curl --fail --silent --show-error --max-time 5 \
      -- "${public_base_url%/}/health" 2>&1 >/dev/null)" && return 0
    sleep 5
  done

  log "The public health endpoint did not become ready: ${last_error:-no response}."
  if caddy_publishes_ports; then
    log 'Caddy does hold ports 80 and 443 on the VM, so this is outside Docker: check the'
    log 'Oracle Cloud VCN or NSG ingress rules, firewalld on the host, and that the'
    log 'PUBLIC_BASE_URL hostname resolves to this VM.'
  else
    log 'Caddy is not holding ports 80 and 443 on the VM, so nothing can reach it.'
  fi
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

    # A backup is not automatically a release that can start: one taken while the server
    # was itself recovering from a truncated .env has no CALLORA_IMAGE to go back to.
    # Restoring the files is still right; pretending the containers came back is not.
    compose config -q 2>/dev/null || {
      log 'The previous configuration was restored but cannot start on its own; the containers were left as they are.'
      return 1
    }

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
  ensure_ports_available || {
    log 'Refusing to deploy; the running release has not been touched.'
    return 1
  }

  backup_current
  install_incoming

  on_error() {
    local exit_code=$?
    local line="$1"
    log "Deployment failed near line $line."
    report_containers
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
    if compose run --rm --no-deps backend migrate; then
      migrated=true
      break
    fi
    log "Migration attempt $attempt failed; retrying."
    sleep 2
  done
  [[ "$migrated" == true ]]

  log 'Replacing the backend only after migrations succeed.'
  compose up -d --no-deps backend
  wait_for_healthy backend 60

  log 'Starting or refreshing the HTTPS reverse proxy.'
  compose up -d --no-deps caddy
  wait_for_healthy caddy 60
  if ! caddy_publishes_ports; then
    # Recreating is what Compose would have done had it known the container was wrong.
    log 'Caddy came up without the host ports; recreating it.'
    compose up -d --no-deps --force-recreate caddy
    wait_for_healthy caddy 60
    caddy_publishes_ports || {
      log 'Caddy still does not publish ports 80 and 443 on this host.'
      return 1
    }
  fi

  compose exec -T backend /usr/local/bin/callora healthcheck

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

# Settings the pipeline owns. It sends them as NAME=VALUE lines on standard input (never
# on a command line); a name that is sent replaces the server's value, an empty value
# clears it, and a name that is not sent is left alone. Anything else is refused, so a
# pipeline bug cannot overwrite host-only settings such as the database password.
readonly SYNCED_SETTINGS=(
  TWILIO_ACCOUNT_SID TWILIO_AUTH_TOKEN STREAM_TOKEN_SECRET
  ELEVENLABS_API_KEY ELEVENLABS_VOICE_ID ELEVENLABS_DYNAMIC_MODEL
  CARTESIA_API_KEY OPENAI_API_KEY TEXT_LLM_MODEL
  TAXI_PHONE_NUMBERS TAXI_HANDOFF_NUMBER
  TAXI_DISPATCH_URL TAXI_DISPATCH_TOKEN TAXI_CRM_URL TAXI_CRM_TOKEN
  ALLOW_LIST ADMIN_API_KEY
)
readonly REQUIRED_SYNCED=(TWILIO_ACCOUNT_SID TWILIO_AUTH_TOKEN)

update_runtime_secrets() {
  local line name value temp_env
  local -A incoming=()

  while IFS= read -r line || [[ -n "$line" ]]; do
    [[ -n "$line" ]] || continue
    name="${line%%=*}"
    value="${line#*=}"
    [[ "$line" == *=* ]] || { log 'update-secrets expects NAME=VALUE lines.'; return 1; }
    case " ${SYNCED_SETTINGS[*]} " in
      *" $name "*) ;;
      *) log "update-secrets refused an unknown setting name: $name"; return 1 ;;
    esac
    [[ "$value" != *$'\r'* ]] || { log "$name must be a single line."; return 1; }
    incoming["$name"]="$value"
  done

  for name in "${REQUIRED_SYNCED[@]}"; do
    [[ -n "${incoming[$name]:-}" ]] || { log "The pipeline did not supply $name."; return 1; }
  done
  [[ "${incoming[TWILIO_ACCOUNT_SID]}" =~ ^AC[0-9a-fA-F]{32}$ ]] || {
    log 'TWILIO_ACCOUNT_SID is not a valid Twilio Account SID.'
    return 1
  }
  [[ -z "${incoming[ADMIN_API_KEY]:-}" || ${#incoming[ADMIN_API_KEY]} -ge 16 ]] || {
    log 'ADMIN_API_KEY must be at least 16 characters when set.'
    return 1
  }
  local e164='\+[1-9][0-9]{7,14}'
  for name in ALLOW_LIST TAXI_PHONE_NUMBERS; do
    value="${incoming[$name]:-}"
    [[ -z "$value" || "$value" =~ ^[[:space:]]*${e164}([[:space:]]*,[[:space:]]*${e164})*[[:space:]]*$ ]] || {
      log "$name must be empty or comma-separated E.164 numbers."
      return 1
    }
  done
  value="${incoming[TAXI_HANDOFF_NUMBER]:-}"
  [[ -z "$value" || "$value" =~ ^${e164}$ ]] || { log 'TAXI_HANDOFF_NUMBER must be one E.164 number.'; return 1; }

  local baseline="$ENV_FILE"
  if [[ ! -f "$ENV_FILE" ]]; then
    log "$ENV_FILE does not exist yet; creating it from the settings the pipeline supplied."
    baseline=/dev/null
  fi

  temp_env="$(mktemp "$APP_DIR/.env.XXXXXX")"
  trap 'rm -f -- "$temp_env"' RETURN
  # Keep every line whose name is not being replaced.
  local replaced
  replaced="$(printf '%s\n' "${!incoming[@]}")"
  awk -v names="$replaced" '
    BEGIN { n = split(names, list, "\n"); for (i = 1; i <= n; i++) if (list[i] != "") drop[list[i]] = 1 }
    { key = $0; sub(/^[[:space:]]*/, "", key); sub(/[[:space:]]*=.*/, "", key); if (!(key in drop)) print }
  ' "$baseline" > "$temp_env"
  for name in "${!incoming[@]}"; do
    printf '%s=%s\n' "$name" "${incoming[$name]}" >> "$temp_env"
  done
  chmod 0600 "$temp_env"
  mv -f -- "$temp_env" "$ENV_FILE"
  trap - RETURN

  # A deployment that ran out of disk used to leave this file truncated, taking the
  # host-only settings with it; they are put back from the server itself.
  recover_missing_settings "${HOST_SETTINGS[@]}"

  local -a missing_host_settings=()
  local setting
  for setting in "${HOST_SETTINGS[@]}"; do
    setting_present "$setting" "$ENV_FILE" || missing_host_settings+=("$setting")
  done
  if [[ ${#missing_host_settings[@]} -gt 0 ]]; then
    log "$ENV_FILE is missing the host-specific settings: ${missing_host_settings[*]}."
    log "Add them on the VM (see 'Production environment on the VM' in DEPLOYMENT.md), then re-run this deployment. The pipeline's settings have already been written."
    return 1
  fi

  # Names only, never values.
  log "Runtime settings updated: ${!incoming[*]}."
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

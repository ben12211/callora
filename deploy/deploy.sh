#!/usr/bin/env bash
# The VM's own settings and housekeeping: /opt/callora/.env (init-host, update-secrets), disk
# space (reclaim) and removing the old Docker Compose install (purge-legacy). Releases are
# k8s-deploy.sh's.
set -Eeuo pipefail
umask 077

readonly APP_DIR=/opt/callora
readonly INCOMING_DIR="$APP_DIR/incoming"
readonly ENV_FILE="$APP_DIR/.env"
readonly ROLLBACK_DIR="$APP_DIR/.rollback"
readonly LAST_IMAGE_FILE="$APP_DIR/.last-successful-image"
readonly LOCK_FILE=/tmp/callora-deploy.lock
# The settings that deliberately never leave the VM, so the pipeline cannot re-send them.
readonly HOST_SETTINGS=(POSTGRES_USER POSTGRES_PASSWORD POSTGRES_DB DATABASE_URL PUBLIC_BASE_URL)
# `name:` in the Compose file. Containers carrying this project label are this release's.
readonly COMPOSE_PROJECT=callora

log() {
  printf '[callora-deploy] %s\n' "$*"
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
  # Never minikube's node: stopped for a day, it is still the cluster Callora runs in.
  docker container prune --force --filter until=24h --filter 'label!=created_by.minikube.sigs.k8s.io' >/dev/null 2>&1 || true

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

# Settings the pipeline owns. It sends them as NAME=VALUE lines on standard input (never
# on a command line); a name that is sent replaces the server's value, an empty value
# clears it, and a name that is not sent is left alone. Anything else is refused, so a
# pipeline bug cannot overwrite host-only settings such as the database password.
readonly SYNCED_SETTINGS=(
  TWILIO_ACCOUNT_SID TWILIO_AUTH_TOKEN STREAM_TOKEN_SECRET
  ELEVENLABS_API_KEY ELEVENLABS_VOICE_ID ELEVENLABS_DYNAMIC_MODEL
  STT_PROVIDER OPENAI_STT_HINTS DEEPGRAM_API_KEY SONIOX_API_KEY OPENAI_API_KEY GEMINI_API_KEY GEMINI_MODEL TEXT_LLM_MODEL TEXT_LLM_REASONING_EFFORT
  AGENT_MODEL AGENT_BACKUP_MODEL AGENT_FALLBACK_MODEL AGENT_REASONING_EFFORT AGENT_PRICES
  TAXI_PHONE_NUMBERS TAXI_HANDOFF_NUMBER
  TAXI_DISPATCH_URL TAXI_DISPATCH_TOKEN TAXI_CRM_URL TAXI_CRM_TOKEN
  ALLOW_LIST ADMIN_API_KEY AUDIO_SAMPLE_NUMBERS DASHBOARD_PASSWORD
  TELEGRAM_API_ID TELEGRAM_API_HASH
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
  [[ -z "${incoming[ADMIN_API_KEY]:-}" || ${#incoming[ADMIN_API_KEY]} -ge 8 ]] || {
    log 'ADMIN_API_KEY must be at least 8 characters when set.'
    return 1
  }
  local e164='\+[1-9][0-9]{7,14}'
  for name in ALLOW_LIST TAXI_PHONE_NUMBERS AUDIO_SAMPLE_NUMBERS; do
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

# First-run host configuration, run by CI before every deployment. Creates only what is
# missing and never overwrites an existing setting:
# - PUBLIC_BASE_URL from the pipeline (a non-secret GitHub Variable), when absent;
# - POSTGRES_USER/DB, a POSTGRES_PASSWORD generated here (it never leaves the VM) and the
#   matching DATABASE_URL, when absent.
# Settings lost from .env are first recovered from the backup or the running containers.
# It refuses to invent a password next to an existing database volume, since that
# password would not match the data.
init_host() {
  local public_base_url='' line
  while IFS= read -r line || [[ -n "$line" ]]; do
    line="${line%$'\r'}"
    case "$line" in
      PUBLIC_BASE_URL=*) public_base_url="${line#PUBLIC_BASE_URL=}" ;;
      '') ;;
      *) log "init-host accepts only PUBLIC_BASE_URL (got ${line%%=*})."; return 1 ;;
    esac
  done

  [[ -f "$ENV_FILE" ]] || { : > "$ENV_FILE"; chmod 0600 "$ENV_FILE"; log "Created $ENV_FILE."; }
  recover_missing_settings "${HOST_SETTINGS[@]}"

  local -a created=()
  if ! setting_present PUBLIC_BASE_URL "$ENV_FILE"; then
    [[ "$public_base_url" =~ ^https://[A-Za-z0-9.-]+(:[0-9]+)?$ ]] || {
      log 'PUBLIC_BASE_URL is not set on this VM; set the GitHub Variable PUBLIC_BASE_URL (https://host, no trailing slash).'
      return 1
    }
    printf 'PUBLIC_BASE_URL=%s\n' "$public_base_url" >> "$ENV_FILE"
    created+=(PUBLIC_BASE_URL)
  fi

  if ! setting_present POSTGRES_PASSWORD "$ENV_FILE"; then
    if docker volume inspect callora_postgres_data >/dev/null 2>&1; then
      log 'POSTGRES_PASSWORD is missing but the callora_postgres_data volume exists.'
      log 'A new password would not open the existing database; restore POSTGRES_* in /opt/callora/.env.'
      return 1
    fi
    local password
    password="$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')"
    setting_present POSTGRES_USER "$ENV_FILE" || { printf 'POSTGRES_USER=callora\n' >> "$ENV_FILE"; created+=(POSTGRES_USER); }
    setting_present POSTGRES_DB "$ENV_FILE" || { printf 'POSTGRES_DB=callora\n' >> "$ENV_FILE"; created+=(POSTGRES_DB); }
    local user db
    user="$(sed -n 's/^[[:space:]]*POSTGRES_USER[[:space:]]*=[[:space:]]*//p' "$ENV_FILE" | tail -n 1)"
    db="$(sed -n 's/^[[:space:]]*POSTGRES_DB[[:space:]]*=[[:space:]]*//p' "$ENV_FILE" | tail -n 1)"
    printf 'POSTGRES_PASSWORD=%s\n' "$password" >> "$ENV_FILE"
    created+=(POSTGRES_PASSWORD)
    if ! setting_present DATABASE_URL "$ENV_FILE"; then
      # Hex only, so no URL encoding is needed.
      printf 'DATABASE_URL=postgresql://%s:%s@db:5432/%s\n' "$user" "$password" "$db" >> "$ENV_FILE"
      created+=(DATABASE_URL)
    fi
  fi

  # The shared secret between the backend and the WhatsApp service; it never leaves the VM.
  if ! setting_present WHATSAPP_TOKEN "$ENV_FILE"; then
    printf 'WHATSAPP_TOKEN=%s\n' "$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')" >> "$ENV_FILE"
    created+=(WHATSAPP_TOKEN)
  fi

  local setting
  local -a missing=()
  for setting in "${HOST_SETTINGS[@]}"; do
    setting_present "$setting" "$ENV_FILE" || missing+=("$setting")
  done
  [[ ${#missing[@]} -eq 0 ]] || { log "Still missing host settings: ${missing[*]}."; return 1; }
  chmod 0600 "$ENV_FILE"

  # Names only, never values.
  if [[ ${#created[@]} -gt 0 ]]; then
    log "Host settings created: ${created[*]}."
  else
    log 'Host settings present.'
  fi
  docker info >/dev/null 2>&1 || { log "The deploy user cannot use Docker; re-run the bootstrap."; return 1; }
  log "Host check passed: Docker $(docker version --format '{{.Server.Version}}' 2>/dev/null), $(available_mib "$APP_DIR") MiB free."
}

# Removes the legacy (pre-V2) Callora deployment so the host can be bootstrapped clean.
# Destructive, so it runs only when asked for explicitly (workflow mode `reset`, which
# requires a typed confirmation). Scope is deliberately narrow:
# - containers whose Compose project label is `callora` (never other projects),
# - images named */callora:* and callora-renikud*,
# - the legacy app volumes: callora_postgres_data (legacy database) and callora_voice_library,
# - Callora's own files in /opt/callora (.env, rollback state, compose/Caddy files, models).
# Caddy's certificate volumes are kept (same domain, no reissue), and nothing belonging to
# another stack on this VM is touched; anything else that looks related is only reported.
purge_legacy() {
  local id
  log 'Removing the legacy Callora deployment.'
  while IFS= read -r id; do
    [[ -n "$id" ]] || continue
    log "Removing container $(docker inspect --format '{{.Name}} ({{.Config.Image}})' "$id" 2>/dev/null)."
    docker rm -f "$id" >/dev/null
  done < <(docker ps --all --quiet --filter "label=com.docker.compose.project=$COMPOSE_PROJECT")

  local image
  while IFS= read -r image; do
    [[ -n "$image" && "$image" != *'<none>'* ]] || continue
    docker image rm -f -- "$image" >/dev/null 2>&1 && log "Removed image $image."
  done < <(docker image ls --format '{{.Repository}}:{{.Tag}}' 2>/dev/null | grep -E '(^|/)callora:|^callora-renikud' || true)

  local volume
  for volume in callora_postgres_data callora_voice_library; do
    if docker volume inspect "$volume" >/dev/null 2>&1; then
      docker volume rm "$volume" >/dev/null && log "Removed volume $volume."
    fi
  done
  docker network ls --format '{{.Name}}' | grep -E "^${COMPOSE_PROJECT}_" | while IFS= read -r net; do
    docker network rm "$net" >/dev/null 2>&1 && log "Removed network $net."
  done

  local f
  for f in .env .env.* docker-compose.prod.yml Caddyfile .rollback .last-successful-image incoming .models; do
    compgen -G "$APP_DIR/$f" >/dev/null || continue
    rm -rf -- "$APP_DIR"/$f
    log "Removed $APP_DIR/$f."
  done
  install -d -m 0700 "$INCOMING_DIR"
  docker image prune --force >/dev/null 2>&1 || true

  # Report, never remove, anything else that mentions callora.
  local leftovers
  leftovers="$(docker ps --all --format '{{.Names}} {{.Image}}' | grep -i callora || true)$(docker volume ls --format '{{.Name}}' | grep -i callora | grep -vE '^callora_caddy_(data|config)$' || true)"
  [[ -z "$leftovers" ]] || log "Left in place (not part of the Callora project; check by hand): $(tr '\n' ' ' <<<"$leftovers")"
  log 'Legacy deployment removed; Caddy certificate volumes and other stacks were kept.'
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
  trap 'log "deploy.sh failed at line $LINENO with exit $?."' ERR

  case "${1:-}" in
    update-secrets)
      [[ $# -eq 1 ]] || { log 'Usage: deploy.sh update-secrets'; exit 2; }
      update_runtime_secrets
      ;;
    purge-legacy)
      [[ $# -eq 2 && "$2" == 'I-UNDERSTAND-THIS-DELETES-THE-CALLORA-DATABASE' ]] || {
        log 'Usage: deploy.sh purge-legacy I-UNDERSTAND-THIS-DELETES-THE-CALLORA-DATABASE'
        exit 2
      }
      purge_legacy
      ;;
    init-host)
      [[ $# -eq 1 ]] || { log 'Usage: deploy.sh init-host'; exit 2; }
      init_host
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
      log 'Usage: deploy.sh {update-secrets|init-host|purge-legacy CONFIRM|reclaim}'
      exit 2
      ;;
  esac
}

main "$@"

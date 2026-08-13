#!/usr/bin/env bash
set -Eeuo pipefail
umask 077

readonly APP_DIR=/opt/callora
readonly INCOMING_DIR="$APP_DIR/incoming"
readonly COMPOSE_FILE="$APP_DIR/docker-compose.prod.yml"
readonly ENV_FILE="$APP_DIR/.env"
readonly CADDY_FILE="$APP_DIR/Caddyfile"
readonly ROLLBACK_DIR="$APP_DIR/.rollback"
readonly LOCK_FILE=/tmp/callora-deploy.lock

log() {
  printf '[callora-deploy] %s\n' "$*"
}

compose() {
  docker compose --env-file "$ENV_FILE" -f "$COMPOSE_FILE" "$@"
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
  install -m 0600 "$INCOMING_DIR/callora.env" "$ENV_FILE"
  install -m 0644 "$INCOMING_DIR/docker-compose.prod.yml" "$COMPOSE_FILE"
  install -m 0644 "$INCOMING_DIR/Caddyfile" "$CADDY_FILE"
  install -m 0755 "$INCOMING_DIR/deploy.sh" "$APP_DIR/deploy.sh"
}

perform_rollback() {
  trap - ERR
  log 'Rolling back application configuration and backend image.'

  if [[ -f "$ROLLBACK_DIR/previous-release" ]]; then
    install -m 0600 "$ROLLBACK_DIR/.env" "$ENV_FILE" || return 1
    install -m 0644 "$ROLLBACK_DIR/docker-compose.prod.yml" "$COMPOSE_FILE" || return 1
    install -m 0644 "$ROLLBACK_DIR/Caddyfile" "$CADDY_FILE" || return 1
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
    install -m 0600 "$ROLLBACK_DIR/.env" "$ENV_FILE" || return 1
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
    head -n 1 > "$APP_DIR/.last-successful-image"
  chmod 0600 "$APP_DIR/.last-successful-image"
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

  IFS= read -r twilio_account_sid
  IFS= read -r twilio_auth_token
  IFS= read -r openai_api_key
  # Optional, and absent when an older workflow sends only three lines.
  allow_list=''
  IFS= read -r allow_list || true

  [[ "$twilio_account_sid" =~ ^AC[0-9a-fA-F]{32}$ ]] || {
    log 'TWILIO_ACCOUNT_SID is not a valid Twilio Account SID.'
    return 1
  }
  [[ "$twilio_auth_token" =~ ^[0-9a-fA-F]{32}$ ]] || {
    log 'TWILIO_AUTH_TOKEN is not a valid Twilio Auth Token.'
    return 1
  }
  [[ -n "$openai_api_key" && "$openai_api_key" != *$'\r'* ]] || {
    log 'OPENAI_API_KEY must be a non-empty, single-line secret.'
    return 1
  }
  # Empty means "no allowlist"; anything else must be E.164 numbers separated by commas.
  [[ -z "$allow_list" || "$allow_list" =~ ^[[:space:]]*\+[1-9][0-9]{7,14}([[:space:]]*,[[:space:]]*\+[1-9][0-9]{7,14})*[[:space:]]*$ ]] || {
    log 'ALLOW_LIST must be empty or a comma-separated list of E.164 numbers.'
    return 1
  }
  [[ -f "$ENV_FILE" ]] || {
    log "$ENV_FILE is missing; create the production application environment before deploying."
    return 1
  }

  temp_env="$(mktemp "$APP_DIR/.env.XXXXXX")"
  trap 'rm -f -- "$temp_env"' RETURN
  awk '
    !/^[[:space:]]*TWILIO_ACCOUNT_SID[[:space:]]*=/ &&
    !/^[[:space:]]*TWILIO_AUTH_TOKEN[[:space:]]*=/ &&
    !/^[[:space:]]*OPENAI_API_KEY[[:space:]]*=/ &&
    !/^[[:space:]]*ALLOW_LIST[[:space:]]*=/
  ' "$ENV_FILE" > "$temp_env"
  printf 'TWILIO_ACCOUNT_SID=%s\n' "$twilio_account_sid" >> "$temp_env"
  printf 'TWILIO_AUTH_TOKEN=%s\n' "$twilio_auth_token" >> "$temp_env"
  printf 'OPENAI_API_KEY=%s\n' "$openai_api_key" >> "$temp_env"
  printf 'ALLOW_LIST=%s\n' "$allow_list" >> "$temp_env"
  chmod 0600 "$temp_env"
  mv -f -- "$temp_env" "$ENV_FILE"
  trap - RETURN
  if [[ -n "$allow_list" ]]; then
    log 'Twilio and OpenAI runtime credentials updated; caller allowlist is active.'
  else
    log 'Twilio and OpenAI runtime credentials updated; no caller allowlist.'
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
    *)
      log 'Usage: deploy.sh {deploy IMAGE|confirm|rollback|update-secrets}'
      exit 2
      ;;
  esac
}

main "$@"

#!/usr/bin/env bash
# Callora on the VM's minikube. CI runs this over SSH as the VM user that owns the cluster
# (its ~/.kube/config); deploy.sh still keeps /opt/callora/.env (init-host, update-secrets),
# which this turns into the callora-env Secret.
#
#   k8s-deploy.sh check                       minikube up, kubectl and Docker usable
#   k8s-deploy.sh deploy IMAGE [WHATSAPP]     roll out a release (WHATSAPP empty: keep it)
#   k8s-deploy.sh confirm                     record the release as the last good one
#   k8s-deploy.sh rollback                    the previous backend and WhatsApp revisions
#   k8s-deploy.sh status                      what runs, for a person
set -Eeuo pipefail
umask 077

readonly APP_DIR=/opt/callora
readonly ENV_FILE="$APP_DIR/.env"
readonly INCOMING_DIR="$APP_DIR/incoming"
readonly MANIFESTS="$APP_DIR/k8s"
readonly EDGE_COMPOSE="$APP_DIR/docker-compose.edge.yml"
readonly EDGE_ENV="$APP_DIR/edge.env"
readonly LAST_IMAGE_FILE="$APP_DIR/.last-successful-image"
readonly LOCK_FILE=/tmp/callora-deploy.lock
readonly NS=callora
# The minikube node (the docker driver's container) and where the volumes live in it: the
# node's /var is the VM's /var/lib/docker/volumes/minikube/_data.
readonly NODE=minikube
readonly NODE_DATA=/var/lib/callora
readonly NODE_PORT=30300

log() { printf '[callora-k8s] %s\n' "$*" >&2; }
k() { kubectl -n "$NS" "$@"; }
on_node() { docker exec "$NODE" "$@"; }
edge() { docker compose -p callora --env-file "$EDGE_ENV" -f "$EDGE_COMPOSE" "$@"; }
setting() { sed -n "s/^[[:space:]]*$1[[:space:]]*=[[:space:]]*//p" "$ENV_FILE" | tail -n 1 | sed -e 's/^"\(.*\)"$/\1/'; }

ensure_cluster() {
  command -v kubectl >/dev/null || { log 'kubectl is not installed on the VM.'; return 1; }
  command -v minikube >/dev/null || { log 'minikube is not installed on the VM.'; return 1; }
  docker info >/dev/null 2>&1 || { log 'This user cannot use Docker.'; return 1; }
  if ! minikube status >/dev/null 2>&1; then
    log 'minikube is not running; starting it.'
    minikube start
  fi
  kubectl get nodes >/dev/null || { log 'kubectl cannot reach the cluster.'; return 1; }
  [[ -f "$ENV_FILE" ]] || { log "$ENV_FILE is missing; run deploy.sh init-host first."; return 1; }
}

# /opt/callora/.env as the callora-env Secret: NAME=VALUE lines only, the last of a name
# wins, quotes around a value go, Compose's image names stay out. Prints the hash of what
# went in, so pods that read it restart when it changes.
apply_settings() {
  local env
  env="$(mktemp)"
  awk '
    /^[[:space:]]*[A-Za-z_][A-Za-z0-9_]*[[:space:]]*=/ {
      line = $0; sub(/^[[:space:]]*/, "", line)
      name = line; sub(/[[:space:]]*=.*/, "", name)
      value = line; sub(/^[^=]*=[[:space:]]*/, "", value)
      if (value ~ /^".*"$/ || value ~ /^\x27.*\x27$/) value = substr(value, 2, length(value) - 2)
      if (name == "CALLORA_IMAGE" || name == "WHATSAPP_IMAGE") next
      if (!(name in seen)) order[++n] = name
      seen[name] = value
    }
    END { for (i = 1; i <= n; i++) print order[i] "=" seen[order[i]] }
  ' "$ENV_FILE" > "$env"
  # The defaults for settings left unset or empty.
  local default name value
  for default in RUST_LOG=info TRANSCRIPT_RETENTION_DAYS=30 ELEVENLABS_API_BASE_URL=https://api.elevenlabs.io \
                 WHATSAPP_MAX_SESSIONS=5; do
    name="${default%%=*}"
    value="$(sed -n "s/^$name=//p" "$env")"
    if [[ -z "$value" ]]; then
      sed -i "/^$name=/d" "$env"
      printf '%s\n' "$default" >> "$env"
    fi
  done
  for name in POSTGRES_USER POSTGRES_PASSWORD POSTGRES_DB DATABASE_URL WHATSAPP_TOKEN; do
    grep -q "^$name=." "$env" || { rm -f -- "$env"; log "$ENV_FILE has no $name."; return 1; }
  done
  kubectl create secret generic callora-env -n "$NS" --from-env-file="$env" --dry-run=client -o yaml |
    kubectl apply -f - >/dev/null
  sha256sum "$env" | cut -c1-16
  rm -f -- "$env"
}

prepare_node_dirs() {
  on_node sh -c "mkdir -p $NODE_DATA/postgres $NODE_DATA/voice-library $NODE_DATA/whatsapp &&
    chown 65532:65532 $NODE_DATA/voice-library && chown 1000:1000 $NODE_DATA/whatsapp"
}

active_calls() {
  local metrics=''
  if k get endpoints backend -o jsonpath='{.subsets[*].addresses[*].ip}' 2>/dev/null | grep -q .; then
    metrics="$(kubectl get --raw "/api/v1/namespaces/$NS/services/backend:http/proxy/metrics" 2>/dev/null || true)"
  fi
  awk '$1 == "callora_calls_active" { print $2 }' <<<"$metrics"
}

# Replacing the backend (or Caddy, which carries the calls' audio) ends calls in progress:
# wait, up to 15 minutes, until none is.
wait_for_calls_to_end() {
  local i active
  for ((i = 0; i < 180; i++)); do
    active="$(active_calls)"
    [[ -z "$active" || "$active" == 0 ]] && return 0
    ((i % 12 == 0)) && log "Waiting for $active call(s) in progress to end."
    sleep 5
  done
  log 'Calls are still in progress after 15 minutes; going on anyway.'
}

# Missing sentences recorded before the release answers calls; a failure (ElevenLabs down)
# does not block it, the sentences then use live TTS.
build_voice_library() {
  local image="$1" job="voice-library-${1##*:}"
  job="${job:0:52}"
  k delete job "$job" --ignore-not-found >/dev/null
  k apply -f - >/dev/null <<EOF
apiVersion: batch/v1
kind: Job
metadata: { name: $job, namespace: $NS }
spec:
  backoffLimit: 0
  activeDeadlineSeconds: 1200
  ttlSecondsAfterFinished: 3600
  template:
    spec:
      restartPolicy: Never
      securityContext: { runAsNonRoot: true, runAsUser: 65532, runAsGroup: 65532 }
      containers:
        - name: build
          image: $image
          args: [voice-library, build]
          envFrom: [{ secretRef: { name: callora-env } }]
          env: [{ name: AUDIO_LIBRARY_DIR, value: /data/voice-library }]
          securityContext: { readOnlyRootFilesystem: true, allowPrivilegeEscalation: false, capabilities: { drop: [ALL] } }
          volumeMounts: [{ name: voice-library, mountPath: /data/voice-library }]
      volumes: [{ name: voice-library, persistentVolumeClaim: { claimName: voice-library } }]
EOF
  log 'Recording new sentences into the voice library.'
  local i succeeded failed
  for ((i = 0; i < 240; i++)); do
    succeeded="$(k get job "$job" -o jsonpath='{.status.succeeded}' 2>/dev/null || true)"
    failed="$(k get job "$job" -o jsonpath='{.status.failed}' 2>/dev/null || true)"
    if [[ "$succeeded" == 1 ]]; then
      k logs "job/$job" --tail=3 2>/dev/null | sed 's/^/  /' || true
      return 0
    fi
    [[ -n "$failed" && "$failed" != 0 ]] && break
    sleep 5
  done
  k logs "job/$job" --tail=30 2>/dev/null || true
  log 'The voice library build did not finish; missing sentences will use live TTS.'
}

current_image() { k get deploy "$1" -o jsonpath='{.spec.template.spec.containers[0].image}' 2>/dev/null || true; }

apply_app() {
  local image="$1" whatsapp="$2" hash="$3"
  sed -e "s|__CALLORA_IMAGE__|$image|g" -e "s|__WHATSAPP_IMAGE__|$whatsapp|g" -e "s|__SETTINGS_HASH__|\"$hash\"|g" \
    "$MANIFESTS/app.yaml" | kubectl apply -f -
}

public_health() {
  local url attempts="${1:-30}" i
  url="$(setting PUBLIC_BASE_URL)"
  [[ "$url" == https://* ]] || { log 'PUBLIC_BASE_URL is not an https address.'; return 1; }
  for ((i = 0; i < attempts; i++)); do
    curl -fsS --max-time 5 -o /dev/null "${url%/}/health" && return 0
    sleep 5
  done
  log "${url%/}/health did not answer."
  return 1
}

wait_for_caddy() {
  local i status
  for ((i = 0; i < 30; i++)); do
    status="$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{end}}' callora-caddy-1 2>/dev/null || true)"
    [[ "$status" == healthy ]] && return 0
    sleep 2
  done
  return 1
}

# Caddy, in Docker, to the backend's NodePort on the node. Recreated only when its
# configuration changed (the first time, or a new minikube address).
refresh_edge() {
  local ip
  ip="$(minikube ip)"
  printf 'PUBLIC_BASE_URL=%s\nCALLORA_UPSTREAM=%s:%s\n' "$(setting PUBLIC_BASE_URL)" "$ip" "$NODE_PORT" > "$EDGE_ENV.next"
  chmod 0600 "$EDGE_ENV.next"
  mv -f -- "$EDGE_ENV.next" "$EDGE_ENV"
  edge config -q
  edge up -d caddy
  wait_for_caddy || { log 'Caddy did not become healthy.'; return 1; }
}

deploy_release() {
  local image="$1" whatsapp="${2:-}" hash
  [[ "$image" =~ ^[a-z0-9]+([._-][a-z0-9]+)*/callora:[0-9a-f]{40}$ ]] || {
    log 'The image must be a Docker Hub Callora image with an immutable commit-SHA tag.'
    return 1
  }
  ensure_cluster
  install -d -m 0700 "$MANIFESTS"
  install -m 0644 "$INCOMING_DIR/k8s/base.yaml" "$INCOMING_DIR/k8s/app.yaml" "$MANIFESTS/"
  install -m 0644 "$INCOMING_DIR/docker-compose.edge.yml" "$EDGE_COMPOSE"
  install -m 0644 "$INCOMING_DIR/Caddyfile" "$APP_DIR/Caddyfile"

  # WhatsApp: the image of this commit; without one (its build failed), whatever runs now.
  [[ -n "$whatsapp" ]] || whatsapp="$(current_image whatsapp)"
  [[ -n "$whatsapp" ]] || whatsapp="${image%%:*}:whatsapp-latest"

  log 'Applying the namespace, the storage and PostgreSQL.'
  prepare_node_dirs
  kubectl create namespace "$NS" --dry-run=client -o yaml | kubectl apply -f - >/dev/null
  hash="$(apply_settings)"
  kubectl apply -f "$MANIFESTS/base.yaml"
  k rollout status deploy/db --timeout=5m

  build_voice_library "$image"
  wait_for_calls_to_end

  log "Rolling out $image."
  apply_app "$image" "$whatsapp" "$hash"
  if ! k rollout status deploy/backend --timeout=6m; then
    k logs deploy/backend --all-containers --tail=60 2>/dev/null || true
    k rollout undo deploy/backend || true
    log 'The backend did not become ready.'
    return 1
  fi
  if ! k rollout status deploy/whatsapp --timeout=4m; then
    log 'The WhatsApp service is not ready; calls and orders are unaffected.'
  fi

  if ! refresh_edge || ! public_health 30; then
    log 'The public address did not answer from the cluster.'
    return 1
  fi
  log "Public health check passed: $(setting PUBLIC_BASE_URL)/health answers from the cluster."

  # Images of releases before, inside the node; whatever a pod uses stays.
  on_node crictl rmi --prune >/dev/null 2>&1 || true
}

confirm_release() {
  current_image backend > "$LAST_IMAGE_FILE"
  chmod 0600 "$LAST_IMAGE_FILE"
  log "Deployment confirmed: $(cat "$LAST_IMAGE_FILE")."
}

rollback_release() {
  log 'Rolling back the backend and the WhatsApp service to their previous revisions.'
  k rollout undo deploy/backend
  k rollout status deploy/backend --timeout=5m
  k rollout undo deploy/whatsapp || true
}

status() {
  kubectl get nodes
  k get pods,svc,pvc -o wide
  log "Active calls: $(active_calls)"
}

main() {
  exec 9>"$LOCK_FILE"
  flock -n 9 || { log 'Another deployment is already running.'; exit 1; }
  trap 'log "k8s-deploy.sh failed at line $LINENO with exit $?."' ERR
  case "${1:-}" in
    check) ensure_cluster && log "Cluster ready: $(kubectl get nodes --no-headers | awk '{print $1, $2, $5}')." ;;
    deploy) [[ $# -ge 2 && $# -le 3 ]] || { log 'Usage: k8s-deploy.sh deploy IMAGE [WHATSAPP_IMAGE]'; exit 2; }
            deploy_release "$2" "${3:-}" ;;
    confirm) confirm_release ;;
    rollback) rollback_release ;;
    status) status ;;
    *) log 'Usage: k8s-deploy.sh {check|deploy IMAGE [WHATSAPP_IMAGE]|confirm|rollback|status}'; exit 2 ;;
  esac
}

main "$@"

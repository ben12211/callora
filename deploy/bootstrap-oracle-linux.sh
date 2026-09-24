#!/usr/bin/env bash
# Prepares an Oracle Linux 9 VM to run Callora in containers. Idempotent: CI runs it on
# every deployment (`sudo -n bash bootstrap-oracle-linux.sh <user>`), so a fresh VM is
# bootstrapped by the first push to main and an existing one is only verified.
# It installs Docker and nothing else; the application only ever runs in containers.
set -Eeuo pipefail

log() { printf '[callora-bootstrap] %s\n' "$*"; }

if [[ $EUID -ne 0 ]]; then
  echo 'Run this script with sudo or as root.' >&2
  exit 1
fi

deploy_user="${1:-opc}"
id "$deploy_user" >/dev/null 2>&1 || { log "Deployment user does not exist: $deploy_user"; exit 1; }

# shellcheck source=/dev/null
source /etc/os-release
if [[ "${ID:-}" != ol || "${VERSION_ID%%.*}" != 9 ]]; then
  log "This bootstrap is written for Oracle Linux 9 (found ${PRETTY_NAME:-unknown})."
  exit 1
fi
[[ "$(uname -m)" == aarch64 ]] || log "Note: $(uname -m) host; the published image is linux/arm64."

fresh_install=false
if command -v docker >/dev/null 2>&1 && docker compose version >/dev/null 2>&1; then
  log "Docker already installed: $(docker --version)."
else
  fresh_install=true
  log 'Installing Docker Engine and the Compose plugin.'
  dnf -y -q install dnf-plugins-core ca-certificates curl util-linux
  dnf config-manager --add-repo https://download.docker.com/linux/centos/docker-ce.repo
  dnf -y -q install docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
fi

# Docker never rotates container logs by default; on a small boot volume that ends as a
# deployment that cannot unpack the next release. An existing file is left alone.
if [[ ! -f /etc/docker/daemon.json ]]; then
  install -d -m 0755 /etc/docker
  cat > /etc/docker/daemon.json <<'JSON'
{
  "log-driver": "json-file",
  "log-opts": { "max-size": "10m", "max-file": "3" }
}
JSON
  chmod 0644 /etc/docker/daemon.json
  # Never bounce a Docker that is already serving production just for log settings.
  if [[ "$fresh_install" == true ]]; then
    systemctl restart docker 2>/dev/null || true
  else
    log 'Wrote /etc/docker/daemon.json; it applies at the next Docker restart (Compose sets log limits per service anyway).'
  fi
fi

systemctl enable --now docker >/dev/null
id -nG "$deploy_user" | tr ' ' '\n' | grep -qx docker || {
  usermod -aG docker "$deploy_user"
  log "Added $deploy_user to the docker group (applies to new SSH sessions)."
}

install -d -m 0750 -o "$deploy_user" -g "$deploy_user" /opt/callora
install -d -m 0700 -o "$deploy_user" -g "$deploy_user" /opt/callora/incoming

if systemctl is-active --quiet firewalld; then
  changed=false
  for service in http https; do
    firewall-cmd --quiet --permanent --query-service="$service" || { firewall-cmd --quiet --permanent --add-service="$service"; changed=true; }
  done
  [[ "$changed" == false ]] || { firewall-cmd --quiet --reload; log 'Opened HTTP/HTTPS in firewalld.'; }
fi

log "Host ready: $(docker --version | cut -d, -f1), $(docker compose version --short 2>/dev/null || echo compose), $(df -h /var | awk 'NR==2 {print $4 " free on /var"}')."
log 'Oracle Cloud ingress (VCN security list / NSG) must allow TCP 80 and 443; the public health check verifies it.'

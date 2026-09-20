#!/usr/bin/env bash
set -Eeuo pipefail

if [[ $EUID -ne 0 ]]; then
  echo 'Run this script with sudo or as root.' >&2
  exit 1
fi

deploy_user="${1:-opc}"
if ! id "$deploy_user" >/dev/null 2>&1; then
  echo "Deployment user does not exist: $deploy_user" >&2
  exit 1
fi

if [[ "$(uname -m)" != aarch64 && "$(uname -m)" != arm64 ]]; then
  echo 'Warning: this server is not ARM64; installation will continue.' >&2
fi

# shellcheck source=/dev/null
source /etc/os-release
if [[ "${ID:-}" != ol || "${VERSION_ID%%.*}" != 9 ]]; then
  echo 'This bootstrap script is intended for Oracle Linux 9.' >&2
  exit 1
fi

dnf -y install dnf-plugins-core ca-certificates curl util-linux
dnf config-manager --add-repo https://download.docker.com/linux/centos/docker-ce.repo
dnf -y install docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin

# Docker's default is to never rotate a container log, which on a boot volume this size
# ends as a deployment that cannot unpack the next release. The Compose file sets this per
# service as well; this covers anything started outside it. An existing file is left alone
# rather than overwritten, since it may have been tuned on purpose.
if [[ ! -f /etc/docker/daemon.json ]]; then
  install -d -m 0755 /etc/docker
  cat > /etc/docker/daemon.json <<'JSON'
{
  "log-driver": "json-file",
  "log-opts": {
    "max-size": "10m",
    "max-file": "3"
  }
}
JSON
  chmod 0644 /etc/docker/daemon.json
fi

systemctl enable --now docker
usermod -aG docker "$deploy_user"

install -d -m 0750 -o "$deploy_user" -g "$deploy_user" /opt/callora
install -d -m 0700 -o "$deploy_user" -g "$deploy_user" /opt/callora/incoming

if systemctl is-active --quiet firewalld; then
  firewall-cmd --permanent --add-service=http
  firewall-cmd --permanent --add-service=https
  firewall-cmd --reload
fi

docker --version
docker compose version

cat <<EOF

Callora host bootstrap is complete.

- Log out and reconnect so $deploy_user receives Docker group membership.
- Keep ports 3000 and 5432 closed publicly.
- Allow inbound TCP 80 and 443 in the Oracle Cloud VCN security list or NSG.
- Point the PUBLIC_BASE_URL hostname to this VM before the first deployment.
EOF

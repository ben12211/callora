#!/usr/bin/env bash
# CI helper: writes the deploy key and host key for SSH to the production VM, and an
# ssh.env that defines SSH/SCP/REMOTE for later steps. Nothing secret is printed.
# Host key: SSH_KNOWN_HOSTS (a pinned `ssh-keyscan` line, recommended) if set, otherwise
# trust-on-first-use via ssh-keyscan, as the legacy pipeline did.
set -Eeuo pipefail
: "${SSH_DIR:?}" "${SERVER_HOST:?SERVER_HOST (GitHub secret IP) is required}" "${SERVER_USER:?SERVER_USER (GitHub secret USER) is required}"
: "${SERVER_SSH_KEY:?the GitHub secret KEY_PEM is required}"

install -d -m 700 "$SSH_DIR"
printf '%s\n' "$SERVER_SSH_KEY" | tr -d '\r' > "$SSH_DIR/deploy_key"
chmod 600 "$SSH_DIR/deploy_key"
ssh-keygen -y -f "$SSH_DIR/deploy_key" >/dev/null 2>&1 || { echo 'KEY_PEM is not a valid, unencrypted private key.' >&2; exit 1; }

if [[ -n "${SSH_KNOWN_HOSTS:-}" ]]; then
  printf '%s\n' "$SSH_KNOWN_HOSTS" > "$SSH_DIR/known_hosts"
else
  ssh-keyscan -T 10 -H "$SERVER_HOST" > "$SSH_DIR/known_hosts" 2>/dev/null
  echo 'Note: host key obtained by ssh-keyscan; set the SSH_KNOWN_HOSTS secret to pin it.'
fi
test -s "$SSH_DIR/known_hosts" || { echo "Could not obtain the SSH host key of the VM (is TCP 22 open?)." >&2; exit 1; }
chmod 600 "$SSH_DIR/known_hosts"

cat > "$SSH_DIR/ssh.env" <<ENV
SSH=(ssh -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes -o ConnectTimeout=15 -o UserKnownHostsFile="$SSH_DIR/known_hosts" -i "$SSH_DIR/deploy_key")
SCP=(scp -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes -o ConnectTimeout=15 -o UserKnownHostsFile="$SSH_DIR/known_hosts" -i "$SSH_DIR/deploy_key")
REMOTE="$SERVER_USER@$SERVER_HOST"
ENV

source "$SSH_DIR/ssh.env"
"${SSH[@]}" "$REMOTE" true || { echo "SSH to $SERVER_USER@<IP> failed: check USER, KEY_PEM and that TCP 22 is open to GitHub runners." >&2; exit 1; }
echo 'SSH to the production VM works.'

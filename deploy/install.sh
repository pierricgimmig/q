#!/usr/bin/env bash
# One-shot VPS install for q serve. Run as root on a Debian or Ubuntu host
# after building or copying the q binary to ./q. Usage:
#   sudo ./deploy/install.sh q.example.com
set -euo pipefail

HOST="${1:?usage: install.sh HOSTNAME}"
BIN="${2:-./q}"

id -u q >/dev/null 2>&1 || useradd --system --home /var/lib/q --shell /usr/sbin/nologin q
install -d -o q -g q -m 750 /var/lib/q
install -m 755 "$BIN" /usr/local/bin/q

# An empty file denies all access until the first token is created.
# Preserve existing credentials on upgrades.
if [ ! -e /var/lib/q/tokens.toml ]; then
  install -o q -g q -m 600 /dev/null /var/lib/q/tokens.toml
fi

sed "s#https://q.example.com#https://${HOST}#" "$(dirname "$0")/q.service" > /etc/systemd/system/q.service
systemctl daemon-reload
systemctl enable q
systemctl restart q

if command -v caddy >/dev/null 2>&1; then
  sed "s#q.example.com#${HOST}#" "$(dirname "$0")/Caddyfile" > /etc/caddy/Caddyfile
  systemctl reload caddy || systemctl restart caddy
else
  echo "caddy is not installed; install it or put another TLS proxy in front of 127.0.0.1:7777" >&2
fi

echo
echo "q serve is running. Create your first token:"
echo "  sudo -u q q --db /var/lib/q/queue.db token create $(logname 2>/dev/null || echo me) --role human"
echo "Then add https://${HOST}/mcp as a custom connector in Grok, Claude, or ChatGPT."

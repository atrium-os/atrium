#!/bin/sh
# Commission Arca's SSH agent (arca.md §9.1) on a host: its account, its
# socket tree, its services.d entry, and its uid in arcad's ARCA_SERVICES.
# Run as root from the repo after building arca/arca-ssh-agent:
#   sh scripts/ssh-agent-service.sh [target-dir]
set -eu
ROOT=$(cd "$(dirname "$0")/.." && pwd)
T=${1:-$ROOT/arca/arca-ssh-agent/target/release}
id _arca-ssh >/dev/null 2>&1 || pw useradd _arca-ssh -u 65047 -d /nonexistent -s /usr/sbin/nologin -c "Arca SSH agent"
install -m 0755 "$T/atrium-ssh-agent" /usr/local/bin/atrium-ssh-agent
install -m 0755 "$T/arca-ssh" /usr/local/bin/arca-ssh
mkdir -p /atrium/sockets/ssh-agent && chown _arca-ssh:_arca-ssh /atrium/sockets/ssh-agent && chmod 755 /atrium/sockets/ssh-agent
mkdir -p /etc/atrium/services.d
install -m 0644 "$ROOT/arca/arca-ssh-agent/etc/services.d/44-atrium-ssh-agent.toml" /etc/atrium/services.d/
J=/var/lib/atrium/jails/ssh-agent
for d in dev libexec lib usr/lib usr/local/bin atrium/sockets/ssh-agent atrium/sockets/arca var/run/atrium etc; do mkdir -p "$J/$d"; done
for f in var/run/atrium/app-registry etc/pwd.db; do
    [ -f "$J/$f" ] || touch "$J/$f"
done
A=/etc/atrium/services.d/42-atrium-arcad.toml
if [ -f "$A" ] && ! grep -q 'ssh-agent=65047' "$A"; then
    sed -i '' -e 's/{ key = "ARCA_SERVICES", value = "\([^"]*\)" }/{ key = "ARCA_SERVICES", value = "\1,ssh-agent=65047" }/' "$A"
    echo "ssh-agent: added to arcad's ARCA_SERVICES; restart arcad"
fi
echo "ssh-agent: commissioned"

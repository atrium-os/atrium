#!/bin/sh
# Build, sign and install nuntius-egress, a `registries` workspace's way out
# (docs/spec/dev-tools.md §5.2, dev-tools-plan.md M8), as a Portcullis
# bundle (org.atrium.nuntius-egress). Runs IN the FreeBSD guest:
#
#   sh nuntius-egress-bundle.sh <nuntius-egress binary> <signing key.pem> [opifex]
#
# portcullisd starts it in a one-shot jail of its own for each running
# `registries` workspace, as _egress (never as the workspace or as root),
# with a listening socket in the workspace's network stack on its stdin.
# The bundle root is the jail's root: the resolver's files are all it reads.
# It is never started by ExecInstance (no callers in its rule).
set -e
BIN=$1 KEY=$2 OPIFEX=${3:-./opifex}
[ -x "$BIN" ] && [ -f "$KEY" ] || { echo "usage: $0 <nuntius-egress> <key.pem> [opifex]"; exit 2; }
B=/root/nuntius-egress-bundle
rm -rf $B && mkdir -p $B/bin $B/etc
cp "$BIN" $B/bin/nuntius-egress
cp /etc/hosts $B/etc/hosts
printf 'hosts: files dns\n' > $B/etc/nsswitch.conf
[ -f /etc/resolv.conf ] && cp /etc/resolv.conf $B/etc/resolv.conf
cat > $B/atrium.toml <<'T'
[app]
id = "org.atrium.nuntius-egress"
name = "Workspace registries proxy"
version = "1"
entry = "bin/nuntius-egress"

[capabilities]
network = "full"

[resources]
memory = "64M"
cpu = 50
T
openssl dgst -sha256 -sign "$KEY" -out $B/atrium.toml.sig $B/atrium.toml
"$OPIFEX" install $B
# Its network is granted once, system-wide: only portcullisd starts it, for
# a `registries` workspace.
portcullis policy grant --system org.atrium.nuntius-egress

id _egress >/dev/null 2>&1 || pw useradd _egress -u 65044 -d /nonexistent -s /usr/sbin/nologin -c "Workspace registries proxy"
P=/etc/atrium/policy.toml
if ! grep -q 'workers."org.atrium.nuntius-egress"' $P 2>/dev/null; then
    cat >> $P <<'T'

[workers."org.atrium.nuntius-egress"]
callers    = []
run_as     = "_egress"
concurrent = 256
T
    chown root:wheel $P && chmod 644 $P
fi
echo "nuntius-egress-bundle: installed $(du -sh $B | cut -f1)"

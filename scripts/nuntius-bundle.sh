#!/bin/sh
# Build, sign and install Nuntius, the system's jailed fetcher, as a
# Portcullis bundle (org.atrium.nuntius, docs/spec/nuntius.md §2 and step 4).
# Runs IN the FreeBSD guest:
#
#   sh nuntius-bundle.sh <nuntiusd binary> <signing key.pem> [opifex] [trust.pem]
#
# An app whose manifest holds `net-fetch` asks portcullisd for a fetcher
# (OpenFetcher); portcullisd starts this bundle in a one-shot jail of its
# own, as _nuntius (never as the app), with the app's channel on its stdin.
# The bundle root is the jail's root: the trust store and the resolver's
# files are read before nuntiusd enters capability mode (casper cap_net is
# then its only way out). It is never started by ExecInstance: portcullisd
# refuses that, so the fetcher's own network grant is no one's raw fetch.
set -e
BIN=$1 KEY=$2 OPIFEX=${3:-./opifex} TRUST=${4:-/etc/ssl/cert.pem}
[ -x "$BIN" ] && [ -f "$KEY" ] && [ -f "$TRUST" ] || { echo "usage: $0 <nuntiusd> <key.pem> [opifex] [trust.pem]"; exit 2; }
B=/root/nuntius-bundle
rm -rf $B && mkdir -p $B/bin $B/etc/ssl
cp "$BIN" $B/bin/nuntiusd
cp "$TRUST" $B/etc/ssl/cert.pem
[ -f $B/etc/hosts ] || cp /etc/hosts $B/etc/hosts
printf 'hosts: files dns\n' > $B/etc/nsswitch.conf
[ -f /etc/resolv.conf ] && cp /etc/resolv.conf $B/etc/resolv.conf
cat > $B/atrium.toml <<'T'
[app]
id = "org.atrium.nuntius"
name = "Nuntius, the system fetcher"
version = "1"
entry = "bin/nuntiusd"

[capabilities]
network = "full"

[resources]
memory = "256M"
cpu = 100
T
openssl dgst -sha256 -sign "$KEY" -out $B/atrium.toml.sig $B/atrium.toml
"$OPIFEX" install $B
# The fetcher's network is granted once, system-wide: only OpenFetcher starts
# it, and only for an app holding net-fetch.
portcullis policy grant --system org.atrium.nuntius

# Its own uid, above the jaild policy's user range (65000) and below nobody.
id _nuntius >/dev/null 2>&1 || pw useradd _nuntius -u 65023 -d /nonexistent -s /usr/sbin/nologin -c "Nuntius fetcher"
P=/etc/atrium/policy.toml
if ! grep -q 'workers."org.atrium.nuntius"' $P 2>/dev/null; then
    # No callers: ExecInstance is refused for this app whatever the rule
    # says; OpenFetcher uses only run_as and the concurrency cap.
    cat >> $P <<'T'

[workers."org.atrium.nuntius"]
callers    = []
run_as     = "_nuntius"
concurrent = 256
T
    chown root:wheel $P && chmod 644 $P
fi
echo "nuntius-bundle: installed $(du -sh $B | cut -f1)"

#!/bin/sh
# Install the per-app GPU executor (docs/spec/tier2-robust-access.md §4).
# Runs IN the FreeBSD guest:
#
#   sh aqueduct-gpu-bundle.sh <aqueduct-gpu-host> <atrium-spv-compile> <signing key.pem> [opifex]
#
# Builds, signs and installs the worker bundle org.atrium.aqueduct.gpu: the
# jail portcullisd starts for each app connection. Its root holds only the two
# binaries; it gets no capabilities (no network, no device node, none of the
# host's files) and a memory cap. It writes no files: compiled shaders are
# mapped straight from the compiler's reply and stay in its memory, per app.
# The compiler runs inside it, in Capsicum capability mode on top.
#
# Also installs the broker (/usr/local/bin/atrium-aqueduct-gpu, run by
# 45-atrium-aqueduct-gpu.toml), its users, its socket dir and the system
# policy's worker rule.
set -e
HOST=$1 COMPILER=$2 KEY=$3 OPIFEX=${4:-./opifex}
[ -x "$HOST" ] && [ -x "$COMPILER" ] && [ -f "$KEY" ] || {
    echo "usage: $0 <aqueduct-gpu-host> <atrium-spv-compile> <key.pem> [opifex]"; exit 2; }
B=/root/aq-gpu-bundle
rm -rf $B && mkdir -p $B/bin
# Started with no arguments, the binary runs as a Tier-2 worker on fd 0
# because of its name (aqueduct-gpu-host/src/bin/main.rs, is_worker_argv0).
cp "$HOST" $B/bin/aqueduct-gpu-worker
cp "$COMPILER" $B/bin/atrium-spv-compile
cat > $B/atrium.toml <<'T'
[app]
id = "org.atrium.aqueduct.gpu"
name = "GPU executor (one per app)"
version = "1"
entry = "bin/aqueduct-gpu-worker"

[resources]
memory = "1G"
cpu = 100

[resources.pool]
weight = 1.0
floor = "64M"
T
openssl dgst -sha256 -sign "$KEY" -out $B/atrium.toml.sig $B/atrium.toml
"$OPIFEX" install $B

install -m 0755 "$HOST" /usr/local/bin/atrium-aqueduct-gpu

id _aqueduct-gpu >/dev/null 2>&1 || pw useradd _aqueduct-gpu -u 65014 -d /nonexistent -s /usr/sbin/nologin -c "aqueduct-gpu broker"
id _aqueduct-gpu-app >/dev/null 2>&1 || pw useradd _aqueduct-gpu-app -u 65015 -d /nonexistent -s /usr/sbin/nologin -c "aqueduct-gpu per-app executor"
install -d -m 0755 -o _aqueduct-gpu -g _aqueduct-gpu /atrium/sockets/aqueduct-gpu
J=/var/lib/atrium/jails/aqueduct-gpu
mkdir -p $J/dev $J/libexec $J/lib $J/usr/lib $J/usr/local/bin $J/atrium/sockets

P=/etc/atrium/policy.toml
if ! grep -q 'workers."org.atrium.aqueduct.gpu"' $P 2>/dev/null; then
    cat >> $P <<'T'

[workers."org.atrium.aqueduct.gpu"]
callers    = ["_aqueduct-gpu"]
run_as     = "_aqueduct-gpu-app"
concurrent = 64
T
    chown root:wheel $P && chmod 644 $P
fi
echo "aqueduct-gpu-bundle: installed $(du -sh $B | cut -f1)"

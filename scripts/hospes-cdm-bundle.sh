#!/bin/sh
# Install hospes-cdm, the content decryption module host, as a Portcullis bundle
# (org.atrium.hospes-cdm, docs/spec/hospes.md §3). Runs IN the FreeBSD guest:
#
#   sh hospes-cdm-bundle.sh <hospes-cdm binary> <signing key.pem> [opifex]
#
# An app whose manifest holds `protected-media` asks portcullisd for a CDM (OpenCdm);
# portcullisd starts this bundle in a one-shot jail of its own, as _hospes-cdm (never as
# the app), and hands it the app's two channels, the library for the key system (from
# /var/db/atrium/cdm/<key system>/current/) and the store for the app and origin (under
# /var/db/atrium/cdm-store/). hospes-cdm is static and reads no file: the bundle is the
# binary alone, and the jail has no network and no capabilities. ExecInstance of it is
# refused, so it runs only for OpenCdm.
#
# hospes_trap.ko must be loaded on the host (hospes-cdm refuses to run without it).
set -e
BIN=$1 KEY=$2 OPIFEX=${3:-./opifex}
[ -x "$BIN" ] && [ -f "$KEY" ] || { echo "usage: $0 <hospes-cdm> <key.pem> [opifex]"; exit 2; }
B=/root/hospes-cdm-bundle
rm -rf $B && mkdir -p $B/bin
cp "$BIN" $B/bin/hospes-cdm
cat > $B/atrium.toml <<'T'
[app]
id = "org.atrium.hospes-cdm"
name = "Protected media module host"
version = "1"
entry = "bin/hospes-cdm"

[capabilities]

[resources]
memory = "768M"
cpu = 100
T
openssl dgst -sha256 -sign "$KEY" -out $B/atrium.toml.sig $B/atrium.toml
"$OPIFEX" install $B

# Its own uid (etc/jaild.policy.toml allows it; 65024-65026 are Vinculum's).
id _hospes-cdm >/dev/null 2>&1 || pw useradd _hospes-cdm -u 65027 -d /nonexistent -s /usr/sbin/nologin -c "Hospes CDM host"
P=/etc/atrium/policy.toml
if ! grep -q 'workers."org.atrium.hospes-cdm"' $P 2>/dev/null; then
    # No callers: ExecInstance is refused for this app whatever the rule says; OpenCdm
    # uses only run_as and the concurrency cap.
    cat >> $P <<'T'

[workers."org.atrium.hospes-cdm"]
callers    = []
run_as     = "_hospes-cdm"
concurrent = 64
T
    chown root:wheel $P && chmod 644 $P
fi
mkdir -p /var/db/atrium/cdm && chmod 755 /var/db/atrium/cdm
echo "hospes-cdm-bundle: installed $(du -sh $B | cut -f1)"

#!/bin/sh
# Build, sign and install the Navigator's page child, navigator-page, as a
# Portcullis bundle (org.atrium.navigator.page, docs/spec/navigator-ui.md
# §2.1 and §14 step 2; limen.md §4). Runs IN the FreeBSD guest:
#
#   sh navigator-page-bundle.sh <navigator-page binary> <signing key.pem> [opifex]
#
# limend starts it with portcullisd's ExecInstance, one jail per page, as
# _navpage (never as limend or the Navigator): the system policy's worker
# rule says so. Its channel to limend is its stdin; frescod reaches it only
# through the embed-only connection limend passes on that channel. Its one
# capability is `navigator-page`: navigatord's ticket socket, where it
# trades a one-shot ticket for the page's recording. No network, no
# graphics socket (so no top-level windows), no files of the host's: the
# fonts are in the binary.
set -e
BIN=$1 KEY=$2 OPIFEX=${3:-./opifex}
[ -x "$BIN" ] && [ -f "$KEY" ] || { echo "usage: $0 <navigator-page> <key.pem> [opifex]"; exit 2; }
APP=org.atrium.navigator.page
B=/root/navigator-page-bundle
rm -rf $B && mkdir -p $B/bin
cp "$BIN" $B/bin/navigator-page
cat > $B/atrium.toml <<T
[app]
id = "$APP"
name = "Navigator page"
version = "1"
entry = "bin/navigator-page"

[capabilities]
navigator-page = true

[resources]
memory = "512M"
cpu = 100
T
openssl dgst -sha256 -sign "$KEY" -out $B/atrium.toml.sig $B/atrium.toml
"$OPIFEX" install $B
# The capability is restricted: granted in the system policy, not by a
# prompt (there is nobody to ask on limend's path).
PATH=$(dirname "$OPIFEX"):$PATH portcullis policy grant --system $APP

# Its own uid, above the jaild policy's user range (65000) and clear of the
# other system workers (_limen 65022, _nuntius 65023).
pw groupshow _navpage >/dev/null 2>&1 || pw groupadd _navpage -g 65035
id _navpage >/dev/null 2>&1 || pw useradd _navpage -u 65035 -g _navpage -d /nonexistent -s /usr/sbin/nologin -c "Navigator page"
P=/etc/atrium/policy.toml
if ! grep -q "workers.\"$APP\"" $P 2>/dev/null; then
    cat >> $P <<T

[workers."$APP"]
callers    = ["_limen"]
run_as     = "_navpage"
concurrent = 64
T
    chown root:wheel $P && chmod 644 $P
fi
echo "navigator-page-bundle: installed $(du -sh $B | cut -f1)"

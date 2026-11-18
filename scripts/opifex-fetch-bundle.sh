#!/bin/sh
# Build, sign and install opifex-fetch, opifexd's network, as the system bundle
# org.atrium.opifex.fetch (docs/spec/opifex.md §7.2, build step C4). Runs IN
# the FreeBSD guest, as root:
#
#   sh opifex-fetch-bundle.sh <opifex-fetch binary> <signing key.pem> [site…]
#
# Its one capability is net-fetch: every byte comes through a Nuntius of its
# own (org.atrium.nuntius must be installed: nuntius-bundle.sh), narrowed by
# net-fetch-sites to the catalog's hosts (GitHub Pages and Releases) plus any
# sites given. opifexd starts it with `portcullis launch --no-prompt`, so it
# gets the system's grant here, once. The key must be on the side-load list
# (/etc/atrium/publishers) until launch trust reads the catalog (C5).
#
#   sh opifex-fetch-bundle.sh --any <opifex-fetch binary> <signing key.pem>
#
# builds the second fetcher, org.atrium.opifex.fetch-any (C7): the same
# binary with net-fetch to any https host, for the catalogs people add, their
# archives, and publishers' domain proofs (DNS TXT through Nuntius, or
# /.well-known/atrium-publisher). opifexd trusts nothing either brings.
set -e
ID=org.atrium.opifex.fetch NAME="Opifex fetcher" B=/root/opifex-fetch-bundle
if [ "${1:-}" = --any ]; then
    ID=org.atrium.opifex.fetch-any NAME="Opifex fetcher (other catalogs)" B=/root/opifex-fetch-any-bundle
    shift
fi
BIN=$1 KEY=$2
[ -x "$BIN" ] && [ -f "$KEY" ] || { echo "usage: $0 [--any] <opifex-fetch> <key.pem> [site…]"; exit 2; }
shift 2
SITES='"atrium-os.github.io", "github.com", "objects.githubusercontent.com", "release-assets.githubusercontent.com"'
for s in "$@"; do SITES="$SITES, \"$s\""; done
SITELINE="net-fetch-sites = [$SITES]"
[ $ID = org.atrium.opifex.fetch-any ] && SITELINE="" SITES="any https host"

rm -rf $B && mkdir -p $B/bin
cp "$BIN" $B/bin/opifex-fetch
cat > $B/atrium.toml <<T
[app]
id = "$ID"
name = "$NAME"
version = "1"
entry = "bin/opifex-fetch"
description = "Brings opifexd the catalog and archives, through Nuntius"

[capabilities]
net-fetch = true
$SITELINE
T
openssl pkcs8 -topk8 -nocrypt -in "$KEY" -out $B.key8 2>/dev/null || cp "$KEY" $B.key8
opifex pack $B --sign $B.key8 >/dev/null
rm -f $B.key8
opifex uninstall $ID >/dev/null 2>&1 || true
opifex install $B
portcullis policy grant --system $ID
echo "opifex-fetch-bundle: $ID installed (sites: $SITES)"

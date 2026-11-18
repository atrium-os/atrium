#!/bin/sh
# The Atrium catalog's publishing step (docs/spec/opifex.md §10.1), run by
# .github/workflows/publish.yml in atrium-os/catalog, or by hand:
#
#   OPIFEX=<opifex binary> sh publish.sh check            # a pull request: check every request
#   OPIFEX=… CATALOG_KEY=<catalog.pem> sh publish.sh publish   # main: append, sign the head
#
# A release request is a file incoming/<name>.toml:
#
#   archive = "https://github.com/atrium-os/catalog-blobs/releases/download/<tag>/<file>.insula"
#   sha256  = "<hex of the archive>"
#   arch    = "aarch64"            # or amd64; "any" for a font package
#
# The archive must already be uploaded. Each request is downloaded, checked
# against its sha256, unpacked, and added with `opifex catalog add`, which
# refuses anything the publisher's key and scopes in site/root.toml don't
# allow (and a manifest whose files aren't all in its signed content
# index). `publish` then moves the request to done/, signs a new head with the
# catalog key (the only key CI holds: it can order and delay, not forge),
# and checks the SDK registry under sdk/ (signed by the platform key at
# release, never here) before it is copied into the site.
#
# The head expires after 14 days, so `publish` also runs weekly to re-sign it.
set -eu
MODE=${1:-check}
OPIFEX=${OPIFEX:-opifex}
SITE=site
[ -f $SITE/root.toml ] || { echo "publish: no $SITE/root.toml (the signed root, from the key ceremony)"; exit 1; }
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
if [ "$MODE" = check ]; then
    cp -R $SITE "$WORK/site"; TARGET="$WORK/site"
else
    TARGET=$SITE
fi
val() { sed -n "s/^$1 *= *\"\(.*\)\"/\1/p" "$2" | head -1; }
n=0
for req in incoming/*.toml; do
    [ -f "$req" ] || continue
    url=$(val archive "$req"); sha=$(val sha256 "$req"); arch=$(val arch "$req")
    case "$url" in https://*) ;; *) echo "publish: $req: archive must be an https URL"; exit 1 ;; esac
    case "$arch" in amd64|aarch64|any) ;; *) echo "publish: $req: arch is amd64, aarch64 or any"; exit 1 ;; esac
    a="$WORK/$n.insula"
    curl -fsSL --proto '=https' -o "$a" "$url"
    got=$( (sha256sum "$a" 2>/dev/null || shasum -a 256 "$a") | cut -d' ' -f1)
    [ "$got" = "$sha" ] || { echo "publish: $req: the archive's sha256 is $got"; exit 1; }
    "$OPIFEX" unarchive "$a" "$WORK/pkg$n" >/dev/null
    "$OPIFEX" catalog add "$TARGET" "$WORK/pkg$n" --arch "$arch" --archive "$a" --url "$url"
    if [ "$MODE" = publish ]; then mkdir -p done; git mv "$req" done/ 2>/dev/null || mv "$req" done/; fi
    n=$((n+1))
done
echo "publish: $n release(s) checked"
if [ -d sdk ]; then
    # Keys allowed to sign the SDK registry: keys/platform/*.pem (public).
    cargo atrium registry verify sdk --keys keys/platform
    if [ "$MODE" = publish ]; then rm -rf $SITE/sdk; cp -R sdk $SITE/sdk; fi
fi
if [ "$MODE" = publish ]; then
    [ -f "${CATALOG_KEY:-}" ] || { echo "publish: CATALOG_KEY must name the catalog key's file"; exit 1; }
    "$OPIFEX" catalog head $SITE --key "$CATALOG_KEY"
fi

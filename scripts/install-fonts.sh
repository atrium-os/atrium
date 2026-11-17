#!/bin/sh
# The system font library (docs/spec/fonts.md): copy the fonts Atrium ships,
# with their licences, into the system folder, read-only.
#
#   sh scripts/install-fonts.sh [dest]        # default /usr/share/fonts/atrium
#
# The folder mirrors the repo's fonts/ tree (atrium-fonts' catalog names files
# by that path). IBM Plex, DejaVu and Phosphor stay where frescod has always
# found them; this is the library's set.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="${1:-/usr/share/fonts/atrium}"
. "$ROOT/scripts/vm-fonts.sh"
mkdir -p "$DEST"
for d in $FONT_LIBRARY_DIRS; do
    rm -rf "${DEST:?}/$d"
    cp -R "$ROOT/fonts/$d" "$DEST/$d"
done
cp "$ROOT/fonts/README.md" "$DEST/README.md"
chmod -R a+rX,go-w "$DEST"
echo "installed $(find "$DEST" -name '*.ttf' | wc -l | tr -d ' ') faces in $DEST"

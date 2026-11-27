#!/bin/sh
# Replay one Run live press on the staged B7 1280x840 layout. See the
# "B7 title check on the Mac" section of docs/spec/background-apps.md.
set -eu
. "$(dirname "$0")/lib/gate.sh"
gate_begin b7-live-title 3
[ "$#" -ge 2 ] && [ "$#" -le 3 ] || gate_abort "usage: $0 VIDEO-LOG-SLICE SCREEN.PNG [VISIT]"
log=$1
screen=$2
visit=${3:-1}
case "$visit" in *[!0-9]*|0*|'') gate_abort "visit must be a positive decimal number";; esac
[ -r "$log" ] && [ -r "$screen" ] || gate_abort "need the log and frame from the same press"

# The UI logs a Rust debug string (the JSON quotes are escaped). Ignore
# titles before the window acknowledges the run, and check its final title.
expected='navigator-ui: live title="live get=ok post=saved{\"n\":1} other-port=refused web=refused visits='"$visit"'"'
if B7_TITLE="$expected" awk '
    /^start-trace navigatord live-start / { starts++; target = ($NF == "http://localhost:9000/live") }
    $0 == "navigator-ui: http://localhost:9000/live runs live (http://localhost:9000)" && target { running = 1 }
    /^navigator-ui: live title=/ && running { title = $0 }
    END { exit !(starts == 1 && running && title == ENVIRON["B7_TITLE"]) }
' "$log"; then
    ok "the running localhost page set the expected UI title"
else
    bad "expected one localhost run ending with $expected"
fi

# Read each visible title separately. The probe also prints its title in
# the document (x >= 249), so a full-frame text search would pass a stale rail.
for tool in magick tesseract; do
    command -v "$tool" >/dev/null || gate_abort "need $tool on the host"
done
[ "$(magick identify -format '%wx%h' "$screen")" = 1280x840 ] || gate_abort "expected the 1280x840 B7 frame"
rail_row() {
    words=$(magick "$screen" -crop "$2" +repage -resize 400% -colorspace gray png:- |
        tesseract stdin stdout --psm 7) || gate_abort "could not read $1"
    case "$words" in
        *"get=ok post=saved"*) ok "$1 shows the script's title";;
        *) bad "$1 read [$words]";;
    esac
}
rail_row "trail heading" '192x26+44+100'
rail_row "page entry" '169x20+44+133'
gate_end

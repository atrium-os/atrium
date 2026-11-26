#!/bin/sh
# Fenestra R1 gate (docs/spec/fenestra.md §5.6, §19): the scene stream on
# the host, no VM and no network.
#
#   1. Random sessions through the Writer and two Readers, 1000 seeds of 150
#      frames, acks dropped at random: every caught-up frame packs to the
#      same bytes on the viewer as on the host.
#   2. The reader's fuzzer, 200 000 hostile streams: no panic, within bounds.
#   3. Recorded sessions (a Pergola app, Navigator, Officina), made by
#      scripts/fenestra-record-vm.sh, replayed through the stream with 30% of
#      acks dropped and with every ack dropped: equal on every frame.
#
#   sh scripts/fenestra-stream-check.sh [recordings-dir]
#
# The recordings default to ~/atrium-vm/fenestra-rec. Runs on Linux or macOS.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REC="${1:-$HOME/atrium-vm/fenestra-rec}"
. "$ROOT/scripts/lib/gate.sh"
gate_begin fenestra-stream 8

cd "$ROOT/fresco-render-core" || gate_abort "no fresco-render-core"
if FENESTRA_STREAM_SEEDS=1000 cargo test -q --release --test stream >"${TMPDIR:-/tmp}/fenestra-stream.log" 2>&1; then
    ok "random sessions equal through the stream (1000 seeds)"
else
    bad "random sessions: $(grep -m1 'panicked' -A1 "${TMPDIR:-/tmp}/fenestra-stream.log" | tail -1)"
fi
if FENESTRA_FUZZ_ITERS=200000 cargo test -q --release --test stream_fuzz >"${TMPDIR:-/tmp}/fenestra-fuzz.log" 2>&1; then
    ok "fuzzer: 200000 hostile streams"
else
    bad "fuzzer: $(grep -m1 'panicked' -A1 "${TMPDIR:-/tmp}/fenestra-fuzz.log" | tail -1)"
fi
cargo build -q --release --bin fenestra-stream-replay 2>/dev/null || gate_abort "fenestra-stream-replay did not build"
REPLAY="$ROOT/fresco-render-core/target/release/fenestra-stream-replay"
# frescod's own text (TEXT_RUN_INSTALL) needs the fonts the guest had, or
# the replay draws no text and tests none.
export ATRIUM_FONT_PATH="$ROOT/fonts/ibm-plex:$ROOT/fonts/phosphor"

for app in pergola navigator sheets; do
    f="$REC/$app.frec"
    if [ ! -s "$f" ]; then
        bad "$app: no recording at $f (sh scripts/fenestra-record-vm.sh)"
        bad "$app: no recording (every ack dropped)"
        continue
    fi
    for drop in 0.3 1.0; do
        line=$("$REPLAY" --viewers 2 --drop-acks "$drop" "$f" | head -1)
        echo "  $line"
        case "$line" in
            *" 0 glyph runs"*) bad "$app ($drop of acks dropped): no text was replayed: $line" ;;
            PASS*) ok "$app: equal with $drop of acks dropped" ;;
            *) bad "$app ($drop of acks dropped): $line" ;;
        esac
    done
done
gate_end

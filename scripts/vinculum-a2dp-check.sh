#!/bin/sh
# Host B5 profile gate, two simulated Classic controllers, no VM or USB.
set -eu
root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
. "$root/scripts/lib/gate.sh"
gate_begin vinculum-a2dp 2
python=${VINCULUM_VENV:-$HOME/.cache/vinculum-venv}/bin/python
[ -x "$python" ] || gate_abort "set VINCULUM_VENV to a venv with bumble installed"
"${FFMPEG:-ffmpeg}" -version >/dev/null 2>&1 || gate_abort "ffmpeg is required as the independent SBC test decoder"
cd "$root/vinculum-atrium-audio"
cargo build --locked --example b5_source || gate_abort "could not build the source"
ok "built the Rust A2DP source with the Lyra adapter"
if "$python" "$root/vinculum/harness/peer_a2dp_sink.py" "${CARGO_TARGET_DIR:-target}/debug/examples/b5_source"; then
    ok "AVDTP, audio-device ring and page, RTP continuity and decoded stereo tones"
else
    bad "source-to-sink gate failed"
fi
gate_end

#!/bin/sh
# H3 on FreeBSD (copied in by scripts/hospes-h3-vm.sh): hospes-cdm against the reference transcript, then the Clear Key clips
# through the player as an unprivileged user. Run as root from this directory.
set -u
D=$(cd "$(dirname "$0")" && pwd)
LIB="$D/${ARCH:-aarch64}-clearkey.so"
rc=0
mkdir -p "$D/store"; chmod 777 "$D/store"
if "$D/bin/hospes-cdm-check" "$D/bin/hospes-cdm" "$LIB" "$D/vectors" "$D/store" > "$D/transcript.txt" 2> "$D/check.err" && diff "$D/vectors/transcript.txt" "$D/transcript.txt"; then
    echo "cdm-check: transcript equals the reference host's ($(wc -l < "$D/transcript.txt") lines)"
else
    echo "cdm-check: MISMATCH"; cat "$D/check.err"; rc=1
fi
chmod -R a+rX "$D"
export ATRIUM_PLAYER_DECODE="$D/bin/atrium-player-decode" ATRIUM_PLAYER_HOSPES_CDM="$D/bin/hospes-cdm" ATRIUM_PLAYER_CDM="$LIB"
export ATRIUM_PLAYER_CLEARKEY=00112233445566778899aabbccddeeff:3a2b1c0d4e5f60718293a4b5c6d7e8f9
for p in vp9-opus-cenc.mp4:vp9-opus-frag.mp4 av1-opus-cbcs.mp4:av1-opus.mp4 vp9-opus-enc.webm:vp9-opus.webm; do
    e=${p%%:*}; c=${p#*:}
    for extra in "" "--kill-worker-after 20"; do
        out=$(su -m nobody -c "$D/bin/atrium-player --headless $extra $D/media/$e" 2>"$D/player.err")
        got=$(echo "$out" | awk '/^video /{print $3}')
        want=$(grep -v '^#' "$D/media/$c.layer.framemd5" | awk -F', *' '{print $NF}')
        sb=$(echo "$out" | grep -c '^sandboxed true')
        rs=$(echo "$out" | grep -c '^worker restarted')
        if [ "$got" = "$want" ] && echo "$out" | grep -q '^ended' && [ "$sb" -ge 1 ]; then
            echo "player $e ${extra:-(plain)}: $(echo "$got" | wc -l | tr -d ' ') frames equal $c's; sandboxed; restarts $rs"
        else
            echo "player $e ${extra:-(plain)}: MISMATCH"; echo "$out" | grep -v '^video' | head; head "$D/player.err"; rc=1
        fi
    done
done
left=$(pgrep -f hospes-cdm | wc -l | tr -d ' ')
echo "hospes-cdm processes left: $left"
exit $rc

#!/bin/sh
# The Player's decoder as a declared worker (app-composition.md §4,
# docs/spec/atrium-player.md §2): atrium-player starts atrium-player-decode
# through atrium-worker's spawn_socket when its bundle's atrium.toml declares
# `[[workers]] decode`, instead of by hand. This checks, for each clip, both
# ways in the running cloud VM (cloud-vm.sh up), as `nobody`:
#   - the frames hash the same as the reference (the .layer.framemd5 files);
#   - the worker reports itself sandboxed (capability mode);
#   - a killed worker is restarted and the clip still ends;
#   - the sandbox probe: the worker can't open, connect or fork;
#   - wall time of a whole headless play, median of RUNS, by hand vs worker.
# Ends "player-worker: N failed".
#
#   sh scripts/vm-player-worker-check.sh
# The guest needs dav1d, libvpx, opus, libvorbis and libogg (pkg), and the
# sysroot their /usr/local copy, as for scripts/hospes-h3-vm.sh.
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
T=x86_64-unknown-freebsd; TU=x86_64_unknown_freebsd
S=${SYSROOT:-$HOME/atrium-vm-amd64/sysroot}
RUNS=${RUNS:-5}
V() { "$ROOT/scripts/vssh" "$@"; }
fails=0
ok() { echo "  ok $1"; }
bad() { echo "  FAIL $1"; fails=$((fails+1)); }

echo "=== cross-build"
X="-C link-arg=--sysroot=$S -C link-arg=-L$S/lib -C link-arg=-L$S/usr/lib -C link-arg=--dynamic-linker=/libexec/ld-elf.so.1 -C link-arg=$S/usr/lib/Scrt1.o -C link-arg=$S/usr/lib/crti.o -C link-arg=$S/usr/lib/crtbeginS.o -C link-arg=-lc -C link-arg=$S/usr/lib/crtendS.o -C link-arg=$S/usr/lib/crtn.o"
(cd "$ROOT/atrium-player" && env PKG_CONFIG_ALLOW_CROSS=1 PKG_CONFIG_SYSROOT_DIR="$S" PKG_CONFIG_LIBDIR="$S/usr/local/libdata/pkgconfig" \
    "CC_$TU=clang --target=$T --sysroot=$S" "CFLAGS_$TU=-I$S/usr/local/include" RUSTFLAGS="$X -C link-arg=-L$S/usr/local/lib" \
    cargo build -q --release --target "$T" -p atrium-player -p atrium-player-decode) || { echo "build failed"; echo "player-worker: 1 failed"; exit 1; }

echo "=== copy"
CLIPS="vp8-vorbis.webm av1-opus.mp4 vp9-10bit.webm red-blue.webm"
P=$(mktemp -d)/pwchk
mkdir -p "$P/bundle/bin" "$P/media"
cp "$ROOT/atrium-player/target/$T/release/atrium-player" "$ROOT/atrium-player/target/$T/release/atrium-player-decode" "$P/bundle/bin/"
llvm-strip "$P"/bundle/bin/* 2>/dev/null || true
cp "$ROOT/atrium-player/atrium.toml" "$P/bundle/"
for c in $CLIPS; do cp "$ROOT/test-assets/media/$c" "$ROOT/test-assets/media/$c.layer.framemd5" "$P/media/"; done
tar czf - -C "$(dirname "$P")" pwchk | V 'rm -rf /tmp/pwchk && tar xzf - -C /tmp && chmod -R a+rX /tmp/pwchk'
rm -rf "$(dirname "$P")"
D=/tmp/pwchk
grep -q '^name *= *"decode"' "$ROOT/atrium-player/atrium.toml" && ok "atrium.toml declares the decode worker" || bad "atrium.toml has no decode worker"

# How each way is started: by hand, ATRIUM_PLAYER_DECODE names the binary;
# as a worker, ATRIUM_APP_ROOT is the bundle whose atrium.toml declares it.
hand="ATRIUM_PLAYER_DECODE=$D/bundle/bin/atrium-player-decode"
work="ATRIUM_APP_ROOT=$D/bundle"
play() { V "su -m nobody -c 'env $1 $D/bundle/bin/atrium-player --headless $2 $D/media/$3' 2>$D/err"; }

echo "=== frames, sandbox, restart"
for c in $CLIPS; do
    want=$(grep -v '^#' "$ROOT/test-assets/media/$c.layer.framemd5" | awk -F', *' '{print $NF}' | tr '\n' ' ')
    for way in hand work; do
        eval env=\$$way
        out=$(play "$env" "" "$c")
        got=$(echo "$out" | awk '/^video /{print $3}' | tr '\n' ' ')
        [ -n "$got" ] && [ "$got" = "$want" ] && ok "$c $way: $(echo "$want" | wc -w | tr -d ' ') frames match" || bad "$c $way: frames differ ($(V "tail -2 $D/err" | tr '\n' ' '))"
        echo "$out" | grep -q '^sandboxed true' && ok "$c $way: sandboxed" || bad "$c $way: not sandboxed"
        out=$(play "$env" "--kill-worker-after 3" "$c")
        echo "$out" | grep -q '^restarted\|^Restarted\|restart' && echo "$out" | grep -q '^ended\|^end' \
            && ok "$c $way: a killed worker restarts and the clip ends" \
            || { echo "$out" | grep -q '^end' && ok "$c $way: the clip ends after a worker kill" || bad "$c $way: no end after a worker kill"; }
    done
done
for way in hand work; do
    eval env=\$$way
    out=$(play "$env" "--probe-sandbox" "vp8-vorbis.webm")
    echo "$out" | grep -q 'probe open-refused=true connect-refused=true fork-refused=true' && ok "$way: the worker can't open, connect or fork" || bad "$way: probe: $(echo "$out" | grep probe)"
done
# The worker way really is atrium-worker's: the supervisor tells it its start on the socket.
out=$(V "su -m nobody -c '$D/bundle/bin/atrium-player-decode </dev/null' 2>&1; echo rc=\$?")
case "$out" in *"no start from the supervisor"*|*"not as a worker"*) ok "the worker refuses to run without a start" ;; *) bad "worker alone: $out" ;; esac

echo "=== timing (whole headless play of vp8-vorbis.webm, $RUNS runs, ms)"
med() { sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}'; }
for way in hand work; do
    eval env=\$$way
    ts=$(V "for i in \$(seq $RUNS); do s=\$(date +%s%N 2>/dev/null || gdate +%s%N); su -m nobody -c 'env $env $D/bundle/bin/atrium-player --headless $D/media/vp8-vorbis.webm' >/dev/null 2>&1; e=\$(date +%s%N); echo \$(( (e-s)/1000000 )); done")
    m=$(echo "$ts" | med); eval t_$way=$m
    echo "  $way: median $m ms ($(echo $ts | tr '\n' ' '))"
done
[ -n "${t_hand:-}" ] && [ -n "${t_work:-}" ] && [ "$t_work" -le $((t_hand + t_hand / 10 + 20)) ] \
    && ok "the worker way is no slower (within 10% + 20 ms)" || bad "the worker way is slower: ${t_work:-?} vs ${t_hand:-?} ms"

echo "player-worker: $fails failed"
[ $fails = 0 ]

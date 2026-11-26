# Shared by the Fenestra VM gates (sourced after scripts/lib/gate.sh): the
# guest's frescod in split mode with the fenestra link, atrium-sheets on a
# small sheet, fenestrad and its session, and the viewer in the container.
#
#   fen_setup              build, stage, start frescod and Sheets
#   fen_fenestrad          start fenestrad in the guest, taking mints on its
#                          socket from the seat's user (root here); sets
#                          FEN_SSH, the viewer's --ssh transport into it
#   compare HOST.png       each placed window's interior on the viewer's
#                          desktop ($W/view/desktop.png) against HOST.png
#
# Needs the VM booted with NETDEV_EXTRA=hostfwd=udp:127.0.0.1:24300-:24300.

fen_setup() {
    set -- env
    . "$ROOT/scripts/cloud-vm.sh" >/dev/null
    set --
    KEY="$HOME/.ssh/fresco_bsd_ed25519"
    SSHOPT="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
    g() { ssh $SSHOPT -o ConnectTimeout=30 -p "${VM_SSH_PORT:-2222}" root@localhost "$@"; }
    put() { scp $SSHOPT -P "${VM_SSH_PORT:-2222}" -q "$1" "root@localhost:$2"; }
    get() { scp $SSHOPT -P "${VM_SSH_PORT:-2222}" -q "root@localhost:$1" "$2"; }
    PORT="${FENESTRA_PORT:-24300}"
    pgrep -fa qemu-system | grep -q "hostfwd=udp:127.0.0.1:$PORT-:$PORT" || gate_abort "the VM has no UDP $PORT forward (NETDEV_EXTRA=hostfwd=udp:127.0.0.1:$PORT-:$PORT sh scripts/cloud-vm.sh up)"
    g true || gate_abort "the VM does not answer ssh"

    echo "=== build"
    S="$HOME/atrium-vm/sysroot"
    export CC_aarch64_unknown_freebsd=clang CFLAGS_aarch64_unknown_freebsd="--target=aarch64-unknown-freebsd --sysroot=$S"
    export AR_aarch64_unknown_freebsd="$(command -v llvm-ar || ls /usr/bin/llvm-ar* 2>/dev/null | head -1)"
    xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" $2) || gate_abort "$1 did not cross-build"; }
    xb frescod "--bin frescod --bin fresco-renderer"
    xb fenestrad ""
    xb fenestra-session "--bin fenestra-session"
    xb atrium-sheets "--bin atrium-sheets"
    (cd "$ROOT/fenestra-view" && cargo build -q --release) || gate_abort "fenestra-view did not build"
    VIEW="$ROOT/fenestra-view/target/release/fenestra-view"
    T="$TARGET/release"
    BINS="$ROOT/frescod/target/$T/frescod $ROOT/frescod/target/$T/fresco-renderer $ROOT/fenestrad/target/$T/fenestrad $ROOT/fenestrad/target/$T/fenestra-shell $ROOT/fenestra-session/target/$T/fenestra-session $ROOT/atrium-sheets/target/$T/atrium-sheets"

    D=/opt/fenestra-desk
    G=/tmp/fendesk
    g "pkill -x fenestrad; pkill -x fenestra-session; pkill -x atrium-sheets; pkill -x frescod; sleep 1; rm -rf $D $G; mkdir -p $D $G; true"
    for b in $BINS; do
        put "$b" "$D/"
        [ "$(sha256sum "$b" | cut -d' ' -f1)" = "$(g "sha256 -q $D/$(basename "$b")")" ] || gate_abort "hash mismatch for $(basename "$b")"
    done
    g "mkdir -p /usr/local/share/fonts/ibm-plex"
    for f in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
        g "test -f /usr/local/share/fonts/ibm-plex/$(basename "$f")" || put "$f" /usr/local/share/fonts/ibm-plex/
    done

    echo "=== host"
    g "cd $G && env FRESCOD_SPLIT=1 FRESCOD_RENDERER_LAUNCHER=unconfined FRESCOD_RENDERER_BIN=$D/fresco-renderer \
        FRESCOD_HEADLESS_PNG=$G/screen FRESCOD_SOCK=$G/fresco.sock FRESCOD_WM_SOCK=$G/wm.sock \
        FRESCOD_FENESTRA_SOCK=$G/fenestra.sock daemon -f -o $G/frescod.log $D/frescod </dev/null"
    i=0; while [ $i -lt 90 ] && ! g "test -S $G/fresco.sock && test -S $G/fenestra.sock"; do sleep 2; i=$((i+1)); done
    g "test -S $G/fenestra.sock" || { g "tail -20 $G/frescod.log"; gate_abort "frescod did not open the fenestra link"; }
    g "chmod 666 $G/fresco.sock"
    CSV=$(mktemp)
    { echo "Region,Q1,Q2,Q3,Q4,Total"; i=2
      for r in North South East West Central Islands Coast Hills; do
          echo "$r,$((i*1130+17)),$((i*1207+3)),$((i*990+411)),$((i*1402+9)),=SUM(B$i:E$i)"; i=$((i+1)); done; } >"$CSV"
    put "$CSV" "$G/sales.csv"; rm -f "$CSV"
    g "cd $G && env HOME=$G FRESCO_SOCKET=$G/fresco.sock daemon -f -o $G/app.log $D/atrium-sheets $G/sales.csv </dev/null"
    sleep "${WAIT:-40}"
}

fen_fenestrad() {
    g "echo root >$G/seat && cd $G && daemon -f -o $G/fenestrad.log $D/fenestrad --frescod $G/fenestra.sock --session $D/fenestra-session \
        --mint-sock $G/mint.sock --seat $G/seat --bind 0.0.0.0:$PORT </dev/null"
    i=0; while [ $i -lt 30 ] && ! g "test -S $G/mint.sock"; do sleep 1; i=$((i+1)); done
    g "test -S $G/mint.sock" || { g "cat $G/fenestrad.log"; gate_abort "fenestrad did not open its mint socket"; }
    # What the viewer runs to mint: ssh into the guest, then fenestra-shell.
    FEN_SSH="ssh $SSHOPT -n -o ConnectTimeout=30 -p ${VM_SSH_PORT:-2222} root@localhost env PATH=$D:/bin:/usr/bin FENESTRA_MINT_SOCK=$G/mint.sock"
}

# Each placed window's interior equal on both screens. Left out: 4 px at its
# edges (frescod's focus ring) and the top STRIP px of the screen (frescod's
# strip): trusted chrome frescod draws over every window, not streamed yet.
STRIP=$(sed -n 's/^pub const STRIP_PT: *[a-z0-9]* *= *\([0-9]*\).*/\1/p' "$ROOT/frescod/src/chrome.rs")
[ -n "$STRIP" ] || gate_abort "no STRIP_PT in frescod/src/chrome.rs"
compare() { python3 -I - "$1" "$W/view/desktop.png" "$V" "$STRIP" <<'EOF'
import sys
from PIL import Image
h = Image.open(sys.argv[1]).convert("RGB")
v = Image.open(sys.argv[2]).convert("RGB")
placed = [l.split()[1:] for l in open(sys.argv[3]) if l.startswith("placed ")]
if h.size != v.size or not placed:
    print(f"sizes {h.size} {v.size}, {len(placed)} placed"); sys.exit()
bad = []
for p in placed:
    i, x, y, w, ht = map(int, p)
    box = (max(x + 4, 0), max(y + 4, int(sys.argv[4])), min(x + w - 4, h.size[0]), min(y + ht - 4, h.size[1]))
    if box[2] <= box[0] or box[3] <= box[1]:
        continue
    a, b = h.crop(box).tobytes(), v.crop(box).tobytes()
    n = sum(1 for k in range(0, len(a), 3) if a[k:k+3] != b[k:k+3])
    if n:
        bad.append(f"window {i}: {n} of {len(a)//3} px differ")
print("equal" if not bad else "; ".join(bad))
EOF
}

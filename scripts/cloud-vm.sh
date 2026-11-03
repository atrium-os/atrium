#!/bin/sh
# FreeBSD dev VM inside a LINUX cloud container (Claude Code cloud sessions).
#
# The Mac dev loop (run-vm.sh, HVF, 9p, ~/src/bsd paths) does not exist here,
# so this is the same loop rebuilt from what a fresh Ubuntu container has:
# QEMU under TCG (no /dev/kvm, so ~50-100x slower than HVF), FreeBSD's own
# published arm64 snapshot image, ssh set up over the serial console, and a
# cross sysroot pulled from the guest itself (the sync-freebsd-sysroot.sh idea:
# no version skew by construction).
#
# Needs download.freebsd.org in the environment's allowed domains.
#
#   sh scripts/cloud-vm.sh setup     # packages, image, first boot, ssh, sysroot, portcullis stack
#   sh scripts/cloud-vm.sh up        # boot an existing image, wait for ssh
#   sh scripts/cloud-vm.sh down      # clean poweroff
#   sh scripts/cloud-vm.sh e2e       # navigator-jail-e2e.sh --quick on a synthetic page
#   . scripts/cloud-vm.sh env        # (sourced) export CROSS_RUSTFLAGS for cross-builds
#
# State lives in $VMDIR (default ~/atrium-vm), never in the tree.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VMDIR="${VMDIR:-$HOME/atrium-vm}"
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SNAPS="https://download.freebsd.org/snapshots/VM-IMAGES/16.0-CURRENT/aarch64"
SMP="${SMP:-$(nproc)}"
MEM="${MEM:-8G}"
SYSROOT="$VMDIR/sysroot"

say() { printf '\n=== %s\n' "$*"; }
die() { printf '\nFAILED: %s\n' "$*" >&2; exit 1; }

cross_env() {
    S="$SYSROOT"
    # ★ Replaces .cargo/config.toml's link args, whose sysroot paths are the
    # Mac's. navigator-jail-e2e.sh applies it only to its cross-build; set as
    # plain RUSTFLAGS it would also link HOST test binaries against FreeBSD's crt.
    export CROSS_RUSTFLAGS="-C link-arg=--sysroot=$S -C link-arg=-L$S/lib -C link-arg=-L$S/usr/lib -C link-arg=--dynamic-linker=/libexec/ld-elf.so.1 -C link-arg=$S/usr/lib/Scrt1.o -C link-arg=$S/usr/lib/crti.o -C link-arg=$S/usr/lib/crtbeginS.o -C link-arg=-lc -C link-arg=$S/usr/lib/crtendS.o -C link-arg=$S/usr/lib/crtn.o"
    export VSSH_CONNECT_TIMEOUT="${VSSH_CONNECT_TIMEOUT:-30}"
}

# A tiny serial-console driver: send each argument as a command, wait for the
# root prompt after it. Python because expect never matched FreeBSD's
# "login: " through a socat pty here, and a stdlib socket has nothing to go wrong.
serial() {
    python3 -I - "$VMDIR/serial.sock" "$@" <<'EOF'
import re, socket, sys, time
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.settimeout(1)
buf = b''
def wait(pat, t):
    global buf
    end = time.time() + t
    while time.time() < end:
        try:
            d = s.recv(4096); buf += d
            sys.stdout.write(d.decode(errors='replace')); sys.stdout.flush()
        except socket.timeout:
            pass
        if re.search(pat, buf):
            buf = b''; return
    sys.exit('serial: timeout waiting for %r' % pat)
s.sendall(b'\r')
wait(rb'(login: |# )$', 900)
for c in sys.argv[2:]:
    s.sendall(c.encode() + b'\r')
    wait(rb'# $', 300)
EOF
}

boot() {
    pgrep -f "file=$VMDIR/vm.qcow2" >/dev/null && { echo "  already running"; return; }
    qemu-system-aarch64 -machine virt,gic-version=3 -accel tcg,thread=multi \
        -cpu max,pauth-impdef=on -smp "$SMP" -m "$MEM" \
        -drive if=pflash,format=raw,unit=0,file="$VMDIR/code.fd",readonly=on \
        -drive if=pflash,format=raw,unit=1,file="$VMDIR/vars.fd" \
        -drive if=virtio,file="$VMDIR/vm.qcow2",format=qcow2,cache=writeback \
        -netdev user,id=net0,hostfwd=tcp:127.0.0.1:2222-:22 -device virtio-net-pci,netdev=net0 \
        -display none \
        -serial unix:"$VMDIR/serial.sock",server,nowait \
        -monitor unix:"$VMDIR/mon.sock",server,nowait \
        > "$VMDIR/qemu.log" 2>&1 &
    sleep 3
    pgrep -f "file=$VMDIR/vm.qcow2" >/dev/null || die "qemu did not start: $(cat "$VMDIR/qemu.log")"
}

wait_ssh() {
    w0=$(date +%s)
    until "$ROOT/scripts/vssh" true >/dev/null 2>&1; do
        [ $(( $(date +%s) - w0 )) -gt 900 ] && die "no ssh after 15 min"
        sleep 5
    done
}

setup() {
    mkdir -p "$VMDIR"
    say "packages"
    if ! command -v qemu-system-aarch64 >/dev/null || ! command -v ssh >/dev/null; then
        sudo apt-get update -qq && sudo apt-get install -y -qq \
            qemu-system-arm qemu-efi-aarch64 qemu-utils openssh-client >/dev/null \
            || die "apt-get install"
    fi
    [ -f "$KEY" ] || ssh-keygen -q -t ed25519 -N '' -f "$KEY"

    say "image"
    if [ ! -f "$VMDIR/vm.qcow2" ]; then
        # ★ Latest/ 404s (RUNBOOK); take the newest dated directory.
        D=$(curl -fsS "$SNAPS/" | grep -oE 'href="[0-9]{8}/"' | tr -dc '0-9\n' | sort | tail -1)
        [ -n "$D" ] || die "cannot list $SNAPS (is download.freebsd.org allowed?)"
        IDX=$(curl -fsS "$SNAPS/$D/")
        IMG=$(echo "$IDX" | grep -oE 'FreeBSD-[^"]*-zfs-[^"]*\.qcow2\.xz' | head -1)
        SUM=$(echo "$IDX" | grep -oE 'CHECKSUM\.SHA256-[^"]*' | head -1)
        echo "  $D/$IMG"
        curl -fsS -o "$VMDIR/$IMG" "$SNAPS/$D/$IMG" || die "download"
        want=$(curl -fsS "$SNAPS/$D/$SUM" | grep "($IMG)" | awk '{print $NF}')
        got=$(sha256sum "$VMDIR/$IMG" | cut -d' ' -f1)
        [ -n "$want" ] && [ "$want" = "$got" ] || die "checksum mismatch: $IMG"
        xz -dT0 "$VMDIR/$IMG" && mv "$VMDIR/${IMG%.xz}" "$VMDIR/vm.qcow2"
        qemu-img resize -q "$VMDIR/vm.qcow2" +20G     # ZFS root grows on first boot
        cp /usr/share/AAVMF/AAVMF_CODE.no-secboot.fd "$VMDIR/code.fd"
        cp /usr/share/AAVMF/AAVMF_VARS.fd "$VMDIR/vars.fd"
        truncate -s 64M "$VMDIR/code.fd" "$VMDIR/vars.fd"
    else
        echo "  $VMDIR/vm.qcow2 exists, kept"
    fi

    say "first boot (TCG: expect ~2.5 min to login)"
    t0=$(date +%s)
    boot
    # ★ The key goes in two halves: the serial console mangled one long echo
    # line and the resulting authorized_keys matched nothing.
    K=$(cut -d' ' -f1,2 "$KEY.pub"); A=$(printf '%s' "$K" | cut -c1-40); B=$(printf '%s' "$K" | cut -c41-)
    serial "root" \
        "sysrc sshd_enable=YES" \
        "mkdir -p /root/.ssh && chmod 700 /root/.ssh" \
        ": > /root/.ssh/authorized_keys" \
        "printf '%s' '$A' >> /root/.ssh/authorized_keys" \
        "printf '%s\n' '$B' >> /root/.ssh/authorized_keys" \
        "chmod 600 /root/.ssh/authorized_keys" \
        "grep -q '^PermitRootLogin' /etc/ssh/sshd_config || echo 'PermitRootLogin prohibit-password' >> /etc/ssh/sshd_config" \
        "service sshd keygen" "service sshd restart" > "$VMDIR/firstboot.log" 2>&1 \
        || die "serial setup (see $VMDIR/firstboot.log)"
    wait_ssh
    echo "  boot + ssh in $(( $(date +%s) - t0 )) s"

    say "guest prep"
    "$ROOT/scripts/vssh" 'mkdir -p /usr/local/bin /usr/local/etc/rc.d /usr/local/lib
        id navtest >/dev/null 2>&1 || pw useradd navtest -m' || die "guest prep"
    echo "  /usr/local dirs, broker user navtest"

    say "cross sysroot from the guest"
    mkdir -p "$SYSROOT"
    "$ROOT/scripts/vssh" 'tar czf - -C / lib usr/lib usr/include 2>/dev/null' | tar xzf - -C "$SYSROOT" 2>/dev/null
    [ -f "$SYSROOT/usr/lib/Scrt1.o" ] || die "sysroot has no Scrt1.o"
    echo "  $(du -sh "$SYSROOT" | cut -f1) in $SYSROOT"

    say "portcullis stack (deploy-portcullis.sh)"
    cross_env
    RUSTFLAGS="$CROSS_RUSTFLAGS" sh "$ROOT/scripts/deploy-portcullis.sh" 2>&1 \
        | grep -v -e '^warning' -e '^ *|' -e '^ \+= ' || true
    "$ROOT/scripts/vssh" 'pgrep -f atrium-jaild >/dev/null' || die "jaild is not running"
}

e2e() {
    cross_env
    # A synthetic page: the real corpus lives on the Mac and must never be
    # copied around (it is other sites' content).
    mkdir -p "$VMDIR/rec-src" "$VMDIR/rec"
    cat > "$VMDIR/rec-src/e2e-fixture.html" <<'EOF'
<!doctype html><html><head><title>Atrium e2e fixture</title>
<style>body{font-family:sans-serif;margin:2em} h1{color:#234} nav.shut{display:none}</style></head>
<body><h1>Atrium jail e2e</h1><p>A small synthetic document for <a href="https://example.org/">the quick lane</a>.</p>
<button id="menu">Menu</button><nav id="nav" class="shut"><a href="/one">one</a></nav>
<ul><li>one</li><li>two</li></ul><script>document.querySelector('p').textContent += ' (script ran)';
document.getElementById('menu').addEventListener('click', function () { var n = document.getElementById('nav'); n.className = n.className === 'shut' ? 'open' : 'shut'; });</script></body></html>
EOF
    (cd "$ROOT/navigator-prerender" && cargo build --quiet --release --bin prerender 2>/dev/null) || die "prerender build"
    # ★ EXPLORE, so the menu's click is recorded as a transition: without
    # one the worker is never asked to navigate, save or restore, and the
    # e2e would pass having exercised only open and close.
    PRERENDER_EXPLORE=1 PRERENDER_EMIT_DIR="$VMDIR/rec" "$ROOT/navigator-prerender/target/release/prerender" \
        "$VMDIR/rec-src/e2e-fixture.html" >/dev/null 2>&1
    [ -f "$VMDIR/rec/e2e-fixture.json" ] || die "prerender wrote no recording"
    sh "$ROOT/scripts/navigator-jail-e2e.sh" --quick --recordings "$VMDIR/rec"
}

case "${1:-}" in
    setup) setup ;;
    up)    mkdir -p "$VMDIR"; boot; wait_ssh ;;
    down)  "$ROOT/scripts/vssh" 'shutdown -p now' >/dev/null 2>&1
           t0=$(date +%s)
           while pgrep -f "file=$VMDIR/vm.qcow2" >/dev/null; do
               [ $(( $(date +%s) - t0 )) -gt 180 ] && { python3 -I -c "import socket,sys;s=socket.socket(socket.AF_UNIX);s.connect(sys.argv[1]);s.sendall(b'quit\n')" "$VMDIR/mon.sock"; break; }
               sleep 3
           done; echo "  down" ;;
    e2e)   e2e ;;
    env)   cross_env ;;
    *)     sed -n '2,20p' "$0"; exit 2 ;;
esac

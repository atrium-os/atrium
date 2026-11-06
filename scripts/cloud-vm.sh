#!/bin/sh
# FreeBSD dev VM inside a LINUX cloud container (Claude Code cloud sessions).
#
# The Mac dev loop (run-vm.sh, HVF, 9p, ~/src/bsd paths) does not exist here,
# so this is the same loop rebuilt from what a fresh Ubuntu container has:
# QEMU under TCG (no /dev/kvm, so ~50-100x slower than HVF), FreeBSD's own
# published snapshot image (arm64, or amd64 with ARCH=amd64), ssh set up over the serial console, and a
# cross sysroot pulled from the guest itself (the sync-freebsd-sysroot.sh idea:
# no version skew by construction).
#
# Needs download.freebsd.org in the environment's allowed domains.
#
#   sh scripts/cloud-vm.sh setup     # packages, image, first boot, ssh, sysroot, portcullis stack
#   sh scripts/cloud-vm.sh up        # boot an existing image, wait for ssh
#   TESSERA_ROOT=1 sh scripts/cloud-vm.sh up   # boot from $VMDIR/devroot.img
#   sh scripts/cloud-vm.sh down      # clean poweroff
#   sh scripts/cloud-vm.sh e2e       # navigator-jail-e2e.sh --quick on a synthetic page
#   sh scripts/cloud-vm.sh lyra      # lyra-vm-check.sh (boot with its qemu-args for sound)
#   . scripts/cloud-vm.sh env        # (sourced) export CROSS_RUSTFLAGS for cross-builds
#
# ARCH=aarch64 (default) or ARCH=amd64 picks the guest. amd64 is the x86_64
# port's VM (docs/spec/x86_64-port.md): q35 + OVMF, still TCG here; on a host
# with /dev/kvm (the R9700 box) it uses KVM automatically.
#
# QEMU_EXTRA=<args> is appended to QEMU's command line (e.g. the sound card
# scripts/lyra-vm-check.sh asks for).
#
# NETDEV_EXTRA=<opts> is appended to the slirp -netdev (e.g. net=, guestfwd=).
# scripts/navigator-fetch-vm-check.sh uses it to put a hermetic "public"
# server on the guest's network: this container has no direct route out.
#
# State lives in $VMDIR (default ~/atrium-vm, ~/atrium-vm-amd64 for amd64),
# never in the tree.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ARCH="${ARCH:-aarch64}"
case "$ARCH" in
    aarch64|arm64) ARCH=aarch64; VMDIR="${VMDIR:-$HOME/atrium-vm}"
                   QEMU=qemu-system-aarch64; RUST_TRIPLE=aarch64-unknown-freebsd ;;
    amd64|x86_64)  ARCH=amd64;   VMDIR="${VMDIR:-$HOME/atrium-vm-amd64}"
                   QEMU=qemu-system-x86_64;  RUST_TRIPLE=x86_64-unknown-freebsd ;;
    *) echo "ARCH must be aarch64 or amd64" >&2; exit 2 ;;
esac
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SNAPS="https://download.freebsd.org/snapshots/VM-IMAGES/16.0-CURRENT/$ARCH"
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
    export TARGET="$RUST_TRIPLE"   # deploy-portcullis.sh / navigator-jail-e2e.sh
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
    if [ "$ARCH" = amd64 ]; then
        if [ -w /dev/kvm ]; then ACCEL="-accel kvm -cpu host"
        # ★ Not -cpu max under TCG: the kernel panics at boot in ZFS's
        # zfs_sha256_transform_avx2 (trap 0xc) — QEMU 8.2's TCG AVX2/XSAVE
        # emulation isn't good enough. Nehalem (SSE4.2, no AVX) boots.
        # TCG_CPU overrides it: Tier-2's x86 code is x86-64-v3 (AVX2, FMA),
        # so running it in the guest needs e.g. TCG_CPU=Haswell-noTSX with a
        # newer QEMU (the atrium-os/qemu fork via QEMU_BIN).
        else ACCEL="-accel tcg,thread=multi -cpu ${TCG_CPU:-Nehalem}"; fi
        MACH="-machine q35 $ACCEL"
    else
        MACH="-machine virt,gic-version=3 -accel tcg,thread=multi -cpu max,pauth-impdef=on"
    fi
    # ICOUNT=<shift>: deterministic virtual time (one instruction = 2^shift
    # ns; idle time skipped). The guest's clock and every device, the sound
    # card included, then run on the same clock however slow the emulation
    # is: for the Lyra clock test (lyra-vm-check.sh clock). Single-threaded.
    if [ -n "${ICOUNT:-}" ]; then
        MACH="$(echo "$MACH" | sed 's/thread=multi/thread=single/') -icount shift=$ICOUNT,sleep=off"
    fi
    # Disks. vm.qcow2 is the ZFS root. A Tessera devroot ($VMDIR/devroot.img,
    # built by scripts/build-tessera-devroot.sh) rides along as the second
    # disk; TESSERA_ROOT=1 puts it FIRST (vtbd0, bootindex 0) so its loader
    # and vfs.root.mountfrom=tessera:/dev/vtbd0p2 take over, with ZFS
    # attached behind it as the fallback.
    ZFSD="-drive file=$VMDIR/vm.qcow2,format=qcow2,cache=writeback,if=none,id=zfsd"
    DEVD="-drive file=$VMDIR/devroot.img,format=raw,cache=writeback,if=none,id=devd"
    if [ -f "$VMDIR/devroot.img" ] && [ "${TESSERA_ROOT:-0}" = 1 ]; then
        DISKS="$DEVD -device virtio-blk-pci,drive=devd,bootindex=0 $ZFSD -device virtio-blk-pci,drive=zfsd"
    elif [ -f "$VMDIR/devroot.img" ]; then
        DISKS="$ZFSD -device virtio-blk-pci,drive=zfsd,bootindex=0 $DEVD -device virtio-blk-pci,drive=devd"
    else
        DISKS="$ZFSD -device virtio-blk-pci,drive=zfsd,bootindex=0"
    fi
    # GPUSIM=1: the gpusim RDNA model as a PCI device (atrium-os/gpusim's
    # gpusim-server + the atrium-os/qemu fork, whose hw/misc/gpusim.c the stock
    # Ubuntu QEMU lacks). Set QEMU_BIN to the fork's build, e.g.
    # ~/atrium-os/qemu/build/qemu-system-x86_64. GPUSIM_DIR: the gpusim checkout.
    GPUSIM_ARGS=""
    if [ "${GPUSIM:-0}" = 1 ]; then
        GPUSIM_DIR="${GPUSIM_DIR:-$HOME/gpusim}"
        GPUSIM_SOCK="${GPUSIM_SOCK:-$VMDIR/gpusim.sock}"
        if [ ! -S "$GPUSIM_SOCK" ] || ! pgrep -f "gpusim-server $GPUSIM_SOCK" >/dev/null; then
            rm -f "$GPUSIM_SOCK"
            SRV="$GPUSIM_DIR/target/release/gpusim-server"
            [ -x "$SRV" ] || (cd "$GPUSIM_DIR" && cargo build -q -p gpusim-server --release) || die "gpusim-server build"
            "$SRV" "$GPUSIM_SOCK" "${GPUSIM_TOPO:-discrete}" > "$VMDIR/gpusim-server.log" 2>&1 &
            i=0; while [ ! -S "$GPUSIM_SOCK" ] && [ $i -lt 50 ]; do sleep 0.1; i=$((i+1)); done
            [ -S "$GPUSIM_SOCK" ] || die "gpusim-server did not listen: $(cat "$VMDIR/gpusim-server.log")"
        fi
        GPUSIM_ARGS="-device gpusim,socket=$GPUSIM_SOCK,id=gpusim0"
    fi
    # shellcheck disable=SC2086
    ${QEMU_BIN:-$QEMU} $MACH -smp "$SMP" -m "$MEM" \
        -drive if=pflash,format=raw,unit=0,file="$VMDIR/code.fd",readonly=on \
        -drive if=pflash,format=raw,unit=1,file="$VMDIR/vars.fd" \
        $DISKS \
        -netdev user,id=net0,hostfwd=tcp:127.0.0.1:2222-:22${NETDEV_EXTRA:+,$NETDEV_EXTRA} -device virtio-net-pci,netdev=net0 \
        $GPUSIM_ARGS ${QEMU_EXTRA:-} \
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
    if [ "$ARCH" = amd64 ]; then PKGS="qemu-system-x86 ovmf"; else PKGS="qemu-system-arm qemu-efi-aarch64"; fi
    if ! command -v $QEMU >/dev/null || ! command -v ssh >/dev/null; then
        # shellcheck disable=SC2086
        sudo apt-get update -qq && sudo apt-get install -y -qq \
            $PKGS qemu-utils openssh-client >/dev/null \
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
        if [ "$ARCH" = amd64 ]; then
            cp /usr/share/OVMF/OVMF_CODE_4M.fd "$VMDIR/code.fd"
            cp /usr/share/OVMF/OVMF_VARS_4M.fd "$VMDIR/vars.fd"
        else
            cp /usr/share/AAVMF/AAVMF_CODE.no-secboot.fd "$VMDIR/code.fd"
            cp /usr/share/AAVMF/AAVMF_VARS.fd "$VMDIR/vars.fd"
            truncate -s 64M "$VMDIR/code.fd" "$VMDIR/vars.fd"
        fi
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

    # SETUP_VM_ONLY=1: stop at a booted, reachable guest (no cross build of
    # the portcullis stack), e.g. on a host that runs prebuilt binaries.
    [ "${SETUP_VM_ONLY:-0}" = 1 ] && return

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
    lyra)  cross_env; shift; sh "$ROOT/scripts/lyra-vm-check.sh" "$@" ;;
    env)   cross_env ;;
    *)     sed -n '2,20p' "$0"; exit 2 ;;
esac

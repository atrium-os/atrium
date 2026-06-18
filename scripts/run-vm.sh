#!/bin/sh
# Boot FreeBSD 16.0-CURRENT arm64 under modified qemu + HVF on macOS.
#
# Usage:
#   run-vm.sh                # headless (no display, ssh in via 2222)
#   run-vm.sh --gpu          # ivshmem-doorbell (requires fresco server running)
#   run-vm.sh --virtio-gpu   # add virtio-gpu-pci (D0 driver bring-up; no UI)
#   run-vm.sh --virtio-gpu --display          # virtio-gpu + Cocoa window
#   run-vm.sh --virtio-gpu --display --tablet # also absolute mouse (more battery cost)
#   run-vm.sh --gpu --virtio-gpu              # both (transitional during D0)
#
# Tunables (env vars):
#   SMP=N        vCPU count, default 4. Lower = less host wakeup overhead
#                when the guest is idle. 2 is plenty for shell + cargo work.
#   MEM=MB       guest RAM, default 12288 (12 GiB).
#
# Power note (laptop battery):
#   Measured 2026-05-03 on this VM: idle qemu sits at ~99% host CPU
#   PER VCPU thread (so smp=4 → ~396% qemu CPU even when the guest's
#   own `top` reports 99% idle). Linear scaling with smp confirmed.
#   Cause: HVF on Apple Silicon doesn't park the vCPU thread when the
#   guest's idle loop issues WFI — the WFI traps to qemu and returns
#   EXCP_HLT, but the vCPU thread doesn't actually block. Real qemu
#   patch needed (target/arm/hvf/hvf.c — a previous patch we thought
#   we had turned out to be the ISV=0 LDP/STP decoder, not WFI).
#
#   Kern.hz on FreeBSD aarch64 already defaults to 100; the "set
#   kern.hz=100 in loader.conf" advice that floats around is for x86
#   guests where the default is 1000 — does not apply here.
#
#   Practical mitigations until the WFI patch lands:
#     pkill -STOP qemu-system-aarch64    # pause when not using
#     pkill -CONT qemu-system-aarch64    # resume
#     SMP=2 run-vm.sh                    # halve the idle burn
#   Display+tablet also add cost: --display ~5%, --tablet ~1-2%.
#   Default headless mode is the most efficient.

set -eu

BSD_DIR="$(cd "$(dirname "$0")/.." && pwd)"
QEMU_DIR="$BSD_DIR/external/qemu-build"
QEMU="$QEMU_DIR/build/qemu-system-aarch64"
SMP="${SMP:-4}"
MEM="${MEM:-12288}"
EFI_SRC="$QEMU_DIR/build/qemu-bundle/opt/homebrew/share/qemu/edk2-aarch64-code.fd"
EFI_PAD="$BSD_DIR/vm/edk2-aarch64-code.fd"
EFI_VARS="$BSD_DIR/vm/edk2-arm-vars.fd"
DISK="$BSD_DIR/vm/vm.qcow2"
# Note: vm.qcow2 is the dev VM with ZFS root (rebuilt 2026-05-09 — see
# RUNBOOK §11). Pristine baseline is vm.qcow2.xz; restore via
# `xz -dk vm/vm.qcow2.xz`. ZFS imports cleanly after `kill -9` —
# fsck-after-hard-kill is not a thing on this image.
SHARE_DIR="$BSD_DIR"

# qemu wants exactly 64MiB for the EFI flash; pad if needed.
if [ ! -f "$EFI_PAD" ]; then
    cp "$EFI_SRC" "$EFI_PAD"
    # pad to 64 MiB
    truncate -s 67108864 "$EFI_PAD"
fi

# Sparse 16 GiB image for atrium-volumes' tessera-backed
# /var/lib/atrium/storage. Created on demand. Inside the guest:
#   kldload tessera_fs && mkfs-tessera --create -s 16384 /dev/vtbd1
#   mount -t tessera /dev/vtbd1 /var/lib/atrium/storage
#   echo "/dev/vtbd1 /var/lib/atrium/storage tessera rw 0 0" >> /etc/fstab
TESSERA_STORAGE="$BSD_DIR/vm/tessera-storage.img"
if [ ! -f "$TESSERA_STORAGE" ]; then
    truncate -s 16G "$TESSERA_STORAGE"
fi
if [ ! -f "$EFI_VARS" ]; then
    truncate -s 67108864 "$EFI_VARS"
fi

# Always blank EFI NVRAM. Adding/removing -device changes PCI addresses,
# which invalidates saved boot entries (EFI drops into UEFI shell). Forcing
# fresh ESP discovery every boot avoids that. Costs ~3s of boot time.
: > "$EFI_VARS"
truncate -s 67108864 "$EFI_VARS"

GPU_ARGS=""
VIRTIO_GPU_ARGS=""
GPUSIM_ARGS=""
AUDIO_ARGS=""   # set by --audio; default empty so `set -u` is happy without it
DISPLAY_FRONTEND=""
WANT_DISPLAY=0
WANT_TABLET=0
WANT_KGDB=0
# Serial on TCP + qemu monitor on a unix socket. Keeps the VM detachable
# (no stdio coupling) while still letting us reach the FreeBSD serial
# console — `nc 127.0.0.1 4444` and send `~^B` to drop into ddb when
# debug.kdb.alt_break_to_debugger=1, or `~B` to send a break when
# debug.kdb.break_to_debugger=1. Use `nc -U /tmp/qmp.sock` for QMP.
NOGRAPHIC="-display none -serial tcp:127.0.0.1:4444,server=on,wait=off -monitor unix:/tmp/qmp.sock,server=on,wait=off"
for arg in "$@"; do
    case "$arg" in
        --gpu)
            SOCK="/tmp/fresco-shmem.sock"
            if [ ! -S "$SOCK" ]; then
                echo "error: $SOCK not found — start karythra-gpu-server first" >&2
                exit 1
            fi
            GPU_ARGS="-chardev socket,path=$SOCK,id=ivshmem \
                      -device ivshmem-doorbell,vectors=2,chardev=ivshmem"
            ;;
        --carillon)
            # Carillon transport (docs/spec/carillon.md): QEMU attaches its
            # ivshmem-doorbell device to aqueduct-gpu-host's IvshmemServer,
            # which must already be listening on this socket. The guest
            # carillon kmod (carillon-kmod/) binds the resulting PCI device.
            SOCK="/tmp/carillon.sock"
            if [ ! -S "$SOCK" ]; then
                echo "error: $SOCK not found — start aqueduct-gpu-host with the" >&2
                echo "       Carillon IvshmemServer on $SOCK first" >&2
                exit 1
            fi
            GPU_ARGS="-chardev socket,path=$SOCK,id=ivshmem \
                      -device ivshmem-doorbell,vectors=2,chardev=ivshmem"
            ;;
        --virtio-gpu)
            VIRTIO_GPU_ARGS="-device virtio-gpu-pci"
            ;;
        --gpusim)
            # Out-of-process gpusim RDNA functional model: a thin pure-C QEMU PCI
            # device talks to the gpusim Rust model over a Unix socket (no Rust
            # linked into QEMU — that hangs macOS in dyld). Starts the server if
            # it isn't already listening. See /Users/girivs/src/gpusim.
            GPUSIM_DIR="/Users/girivs/src/gpusim"
            GPUSIM_SOCK="${GPUSIM_SOCK:-/tmp/gpusim.sock}"
            GPUSIM_TOPO="${GPUSIM_TOPO:-discrete}"
            if [ ! -S "$GPUSIM_SOCK" ]; then
                SRV="$GPUSIM_DIR/target/release/gpusim-server"
                if [ ! -x "$SRV" ]; then
                    ( cd "$GPUSIM_DIR" && cargo build -p gpusim-server --release )
                fi
                echo "starting gpusim-server on $GPUSIM_SOCK ($GPUSIM_TOPO)"
                "$SRV" "$GPUSIM_SOCK" "$GPUSIM_TOPO" >/tmp/gpusim-server.log 2>&1 &
                # wait briefly for the socket
                i=0; while [ ! -S "$GPUSIM_SOCK" ] && [ $i -lt 50 ]; do sleep 0.1; i=$((i+1)); done
            fi
            GPUSIM_ARGS="-device gpusim,socket=$GPUSIM_SOCK"
            ;;
        --venus)
            # ⚠ SUPERSEDED — venus (guest Vulkan proxied to host MoltenVK over the
            # virtio-gpu venus capset) was the original GPU-VM transport. It is
            # replaced by --carillon (the ivshmem-doorbell paravirt path to
            # aqueduct-gpu-host → MoltenVK → Metal). Kept only for historical
            # comparison; new work uses --carillon for the host GPU, or the Tier-2
            # software Vulkan ICD (atrium-vk-icd) for pure in-VM CPU rendering.
            #
            # virgl_render_server (spawned by virglrenderer when venus is
            # active) needs to find MoltenVK on macOS. brew installs the
            # ICD JSON at $(brew --prefix)/etc/vulkan/icd.d/, outside the
            # Vulkan loader's default search path — point at it directly.
            # The library_path inside the JSON is "../../../lib/libMoltenVK.dylib"
            # which resolves correctly because the JSON sits at
            # $(brew --prefix)/etc/vulkan/icd.d/.
            BREW_PREFIX="$(brew --prefix 2>/dev/null || echo /opt/homebrew)"
            export VK_ICD_FILENAMES="$BREW_PREFIX/etc/vulkan/icd.d/MoltenVK_icd.json"
            export VK_DRIVER_FILES="$VK_ICD_FILENAMES"
            # Also expose MoltenVK to the bare-basename dlopen path
            # in case the Vulkan loader's JSON resolution doesn't reach
            # virgl_render_server (which is fork'd from QEMU).
            export DYLD_LIBRARY_PATH="$BREW_PREFIX/lib${DYLD_LIBRARY_PATH:+:$DYLD_LIBRARY_PATH}"
            # Optional MoltenVK knobs (uncomment as needed):
            #   MVK_CONFIG_SYNCHRONOUS_QUEUE_SUBMITS=1  - commit + wait
            #     Metal cmd buffers inside vkQueueSubmit (debug only).
            #   MVK_CONFIG_TRACE_VULKAN_CALLS=1         - print every
            #     Vulkan call landed on MoltenVK's side to QEMU stderr.
            #   MVK_CONFIG_LOG_LEVEL=4                  - verbose log.
            # All defaulted off in production; the venus stack works
            # without any of them once atrium-os/MoltenVK
            # MVKDeviceMemory makeResident fix is installed.
            # Capture the render server's own log output to a per-pid
            # file. Without this its messages may end up in macOS's
            # unified log (via syslog) where they're hard to find.
            export VIRGL_LOG_FILE="/tmp/virgl-%PID%.log"
            export VIRGL_LOG_LEVEL=debug
            # Two GPU devices, by design:
            #
            #  bochs-display: gives EDK2 a real GOP that the loader
            #    captures into MODINFOMD_EFI_FB, so scfb(4) /
            #    atrium-bootfb pick up an early-boot framebuffer for
            #    splash screen. Boots without it but loses splash.
            #
            #  virtio-gpu-gl-pci,venus=on: the (superseded) venus GPU —
            #    guest Vulkan proxied to host MoltenVK via the venus capset.
            #    blob+hostmem are required by venus. Carillon replaced this.
            #
            # Requires the Atrium-patched QEMU + virglrenderer (-Dvenus=true)
            # plus the Atrium-patched EDK2 that drops VirtioGpuDxe (so the
            # firmware doesn't probe the GL/venus device and hang the boot).
            # bochs-display dropped: with the BLOB-pixman scanout path
            # (atrium qemu patch), virtio-gpu-gl-pci is the sole display
            # post-handoff. EDK2 boot splash falls back to virtio-gpu's
            # own framebuffer, which Cocoa renders fine via the same
            # pixman path (no GOP needed for the splash on macOS host).
            VIRTIO_GPU_ARGS="-device virtio-gpu-gl-pci,venus=on,blob=on,hostmem=512M,id=atrium-gpu"
            ;;
        --bochs)
            # bochs-display *should* expose BochsDisplayDxe → GOP, but in
            # practice this EDK2 build often boots it with "Guest has not
            # initialized the display" (no GOP metadata, vt has no efifb).
            # Prefer --ramfb, which is the VERIFIED scfb surface for this VM
            # (efifb 800x600 → atrium-bootfb → atrium-splash, confirmed).
            VIRTIO_GPU_ARGS="-device bochs-display"
            ;;
        --ramfb)
            # ramfb — QEMU's purpose-built simple framebuffer for EFI/early
            # boot. EDK2's QemuRamfbDxe publishes a GOP from it (no mode
            # programming; the fb lives in guest RAM, QEMU scans it out), which
            # loader.efi captures into MODINFOMD_EFI_FB → FreeBSD vt_efifb →
            # atrium-bootfb. The cleanest "scfb" surface for the VM.
            VIRTIO_GPU_ARGS="-device ramfb"
            ;;
        --display)
            # Cocoa window so virtio-gpu scanout is actually visible.
            # Drops -nographic; serial console moves to mon:stdio.
            # Adds USB xhci + usb-kbd so keyboard input flows through
            # (ukbd → kbdmux0 → /dev/input/event0, what
            # frescod's input_reader thread reads). Add
            # --tablet on top for absolute-coordinate mouse, which is
            # nicer UX but costs ~1–2% host CPU continuously polling.
            WANT_DISPLAY=1
            ;;
        --tablet)
            WANT_TABLET=1
            ;;
        --kgdb)
            # Enable QEMU gdb stub on tcp::1234.  Attach from host
            # with: scripts/kgdb-attach.sh
            # Live single-step + breakpoints into the running kernel
            # using aarch64-elf-gdb against the LAMINAR-DEV
            # kernel.full (with debug symbols).
            WANT_KGDB=1
            ;;
        --audio)
            # Intel HD Audio (PCI, attaches on the virt machine) → coreaudio
            # host backend, so guest sound plays live on the Mac. FreeBSD's
            # snd_hda driver matches intel-hda. For headless/deterministic
            # capture set AUDIO_BACKEND=wav,path=/tmp/lyra.wav (no live sound;
            # inspect the file). Lyra (docs/spec/atrium-lyra-architecture.md).
            AUDIO_BACKEND="${AUDIO_BACKEND:-coreaudio}"
            AUDIO_ARGS="-audiodev ${AUDIO_BACKEND},id=snd0 \
                        -device intel-hda \
                        -device hda-output,audiodev=snd0"
            ;;
        *)
            echo "error: unknown arg $arg" >&2
            exit 1
            ;;
    esac
done

# Compose display frontend after arg parse so --tablet works regardless
# of the flag order on the command line.
if [ "$WANT_DISPLAY" = 1 ]; then
    # Keep TCP serial + QMP socket (same as headless NOGRAPHIC) so ddb
    # remains reachable while the Cocoa window is up. Previously the
    # display branch routed serial to mon:stdio, which clobbers
    # ddb_session.py — defeats the point of "panic in display path"
    # debugging. Use `nc 127.0.0.1 4444` for serial / break with
    # `~^B`; QMP at /tmp/qmp.sock for screendumps etc.
    DISPLAY_FRONTEND="-display cocoa \
                      -serial tcp:127.0.0.1:4444,server=on,wait=off \
                      -monitor unix:/tmp/qmp.sock,server=on,wait=off \
                      -device qemu-xhci \
                      -device usb-kbd"
    if [ "$WANT_TABLET" = 1 ]; then
        DISPLAY_FRONTEND="$DISPLAY_FRONTEND -device usb-tablet"
    fi
    NOGRAPHIC=""
elif [ "$WANT_TABLET" = 1 ]; then
    echo "error: --tablet requires --display" >&2
    exit 1
fi

KGDB_ARGS=""
if [ "$WANT_KGDB" = 1 ]; then
    # -s = shorthand for -gdb tcp::1234, host-side.
    # NOTE: with QEMU 10.x + HVF the guest starts PAUSED when -s is
    # present (undocumented; differs from KVM behaviour).  Send
    # `cont` via the QMP socket once it's up so boot proceeds.
    KGDB_ARGS="-s"
    echo "kgdb: gdb stub on 127.0.0.1:1234; attach with scripts/kgdb-attach.sh"
    (
        # Wait for QMP socket then resume guest.
        until [ -S /tmp/qmp.sock ]; do sleep 0.2; done
        sleep 0.5
        echo "cont" | nc -U /tmp/qmp.sock -w 1 >/dev/null 2>&1 || true
    ) &
fi

# HVF latency: clear background/throttled QoS on this process so macOS does
# not park the vCPU threads on the E-cores (or deschedule them) under
# contention — the source of the episodic ~100 ms host stalls that confound
# in-VM latency/audio measurement (gapdet: ~60x fewer gaps un-throttled;
# Lyra --feed at 8 ms: lane underruns 21 -> 2). exec inherits this into QEMU.
# A priority boost (nice < 0) needs root — for the tightest path also run:
#   sudo renice -10 -p $(pgrep -n qemu-system-aarch64)
taskpolicy -B -p $$ 2>/dev/null || true

exec "$QEMU" \
    -L "$QEMU_DIR/pc-bios" \
    ${ATRIUM_QEMU_TRACE:+-trace events=$ATRIUM_QEMU_TRACE} \
    -d guest_errors -D /tmp/qemu-guest-errors.log \
    -accel hvf -cpu host -machine virt,gic-version=3 \
    -smp "$SMP" -m "$MEM" \
    -drive if=pflash,format=raw,unit=0,file="$EFI_PAD",readonly=on \
    -drive if=pflash,format=raw,unit=1,file="$EFI_VARS" \
    -drive if=virtio,file="$DISK",format=qcow2,cache=writeback \
    -drive file="$BSD_DIR/vm/crash-test.img",format=raw,cache=writeback,if=none,id=crashdrv \
    -device virtio-blk-pci,drive=crashdrv,serial=tessera-crashtest,config-wce=on \
    -drive file="$BSD_DIR/vm/tessera-storage.img",format=raw,cache=writeback,if=none,id=storagedrv \
    -device virtio-blk-pci,drive=storagedrv,serial=atrium-storage,config-wce=on \
    -device virtio-net-pci,netdev=net0 \
    -netdev user,id=net0,hostfwd=tcp::2222-:22 \
    -fsdev local,id=share,path="$SHARE_DIR",security_model=none \
    -device virtio-9p-pci,fsdev=share,mount_tag=bsd_share \
    -fsdev local,id=mesa,path="$BSD_DIR/external/mesa",security_model=none \
    -device virtio-9p-pci,fsdev=mesa,mount_tag=mesa_share \
    $NOGRAPHIC \
    $DISPLAY_FRONTEND \
    $GPU_ARGS \
    $VIRTIO_GPU_ARGS \
    $GPUSIM_ARGS \
    $AUDIO_ARGS \
    $KGDB_ARGS

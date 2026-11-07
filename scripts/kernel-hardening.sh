#!/bin/sh
# Kernel hardening (review 2026-11-07 step 0): install and verify on a guest.
#
#   sh scripts/kernel-hardening.sh install   # copy etc/sysctl.atrium.conf to the guest, apply it
#   sh scripts/kernel-hardening.sh check     # verify kernel config, sysctls, absent modules
#
# Runs against the VM through scripts/vssh (cloud-vm.sh / run-vm.sh guests).
# `check` exits non-zero on the first class of failure and prints every
# mismatch it saw.  It is meant to run after booting an ATRIUM* kernel built
# from atrium-os/freebsd-src (sys/conf/std.atrium); on GENERIC it reports
# what the ATRIUM config would have removed.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VSSH="$ROOT/scripts/vssh"
CONF="$ROOT/etc/sysctl.atrium.conf"

install_conf() {
    # rc.d/sysctl reads /etc/sysctl.conf and then /etc/sysctl.conf.local.
    "$VSSH" 'cat > /etc/sysctl.conf.local' < "$CONF" || exit 1
    "$VSSH" 'service sysctl restart >/dev/null 2>&1; true'
    echo "installed /etc/sysctl.conf.local"
}

check() {
    fail=0
    # 1. sysctls: every key=value in the conf file.  A key the kernel does not
    # have is fine only for kern.ipc.tls.enable (KERN_TLS compiled out).
    grep -E '^[a-z][a-z0-9_.]*=' "$CONF" | while IFS='=' read -r k v; do
        got=$("$VSSH" "sysctl -n $k 2>/dev/null" | tr -d '\r')
        if [ -z "$got" ]; then
            case "$k" in kern.ipc.tls.enable) echo "ok    $k absent (no KERN_TLS)";;
                         *) echo "FAIL  $k missing"; echo x >> /tmp/khard.$$;; esac
        elif [ "$got" != "$v" ]; then
            echo "FAIL  $k = $got (want $v)"; echo x >> /tmp/khard.$$
        else
            echo "ok    $k = $v"
        fi
    done
    [ -s /tmp/khard.$$ ] && fail=1; rm -f /tmp/khard.$$

    # 2. kernel config: options that must be absent (INCLUDE_CONFIG_FILE puts
    # the expanded config in kern.conftxt).
    ident=$("$VSSH" 'uname -i' | tr -d '\r')
    echo "kernel ident: $ident"
    conf=$("$VSSH" 'sysctl -n kern.conftxt' 2>/dev/null)
    for o in COMPAT_FREEBSD32 SYSVSHM SYSVMSG SYSVSEM SCTP_SUPPORT KERN_TLS \
             TCP_OFFLOAD IPSEC_SUPPORT NFSCL NFSD NFS_ROOT GEOM_RAID EVDEV_SUPPORT \
             COMPAT_LINUXKPI XENHVM; do
        if printf '%s\n' "$conf" | grep -qE "^options[[:space:]]+$o([[:space:]=]|\$)"; then
            echo "FAIL  options $o is compiled in"; fail=1
        else
            echo "ok    no options $o"
        fi
    done
    for d in netmap evdev uinput wlan hyperv mlx5; do
        if printf '%s\n' "$conf" | grep -qE "^device[[:space:]]+$d\$"; then
            echo "FAIL  device $d is compiled in"; fail=1
        else
            echo "ok    no device $d"
        fi
    done

    # 3. modules that must not be in the kernel's module directory.
    kdir=$("$VSSH" 'sysctl -n kern.bootfile' | tr -d '\r'); kdir=${kdir%/*}
    for m in linux linux64 linux_common linuxkpi sctp tcp_rack tcp_bbr ipfw \
             ipdivert libalias netgraph ng_ubt ng_hci nfscl nfsd krpc kgssapi \
             fusefs netmap ctl cryptodev vmm sysvshm sysvmsg sysvsem ipsec mlx5; do
        if "$VSSH" "test -e $kdir/$m.ko"; then
            echo "FAIL  $kdir/$m.ko is installed"; fail=1
        else
            echo "ok    no $m.ko"
        fi
    done

    # 4. INIT_ALL and branch protection are compile-time: report what the
    # build recorded (kern.conftxt has the makeoptions line).
    if printf '%s\n' "$conf" | grep -q 'INIT_ALL=zero'; then
        echo "ok    makeoptions INIT_ALL=zero"
    else
        echo "FAIL  INIT_ALL=zero not in kern.conftxt"; fail=1
    fi
    case "$("$VSSH" 'uname -m' | tr -d '\r')" in
        arm64) pac=$("$VSSH" 'sysctl -n hw.pac.enable 2>/dev/null; dmesg | grep -m1 -iE "PAC|ptrauth"' | tr -d '\r')
               echo "info  arm64 PAC/BTI: kernel built -mbranch-protection=standard; $pac" ;;
    esac
    echo "info  kernel ASLR: none on FreeBSD (fixed KERNBASE on amd64 and arm64)"
    [ $fail = 0 ] && echo "PASS" || echo "FAILED"
    return $fail
}

case "${1:-}" in
    install) install_conf ;;
    check)   check ;;
    *)       sed -n '2,12p' "$0"; exit 2 ;;
esac

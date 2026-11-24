#!/bin/sh
# Run as root on a disposable FreeBSD Tessera guest. Stage the host-built
# native_memory and workbook_replace_check examples in PATH. The argument
# is a new directory on the test volume (the check leaves its files there).
set -eu
if [ "$#" -ne 1 ]; then
    echo "usage: $0 NEW_TESSERA_DIR" >&2
    exit 2
fi
mkdir "$1"
cd "$1"
test "$(df -T . | awk 'NR == 2 {print $2}')" = tessera
old_pressure=$(sysctl -n kern.tessera.gc_pressure_pct)
native_memory make dense base.osheet 7
native_memory make dense edited.osheet 8
expected=$(sha256 -q edited.osheet)
workbook_replace_check results base.osheet edited.osheet

scans=$(sysctl -n kern.tessera.gc_scans)
grows=$(sysctl -n kern.tessera.gc_manifest_stack_grows)
reclaimed=$(sysctl -n kern.tessera.gc_reclaimed)
trap 'sysctl "kern.tessera.gc_pressure_pct=$old_pressure" >/dev/null' EXIT
trap 'exit 1' HUP INT TERM
sysctl kern.tessera.gc_pressure_pct=50

# A scan starts before marking. Wait for both a wider walk and actual
# reclamation before the first comparison. Keep the kernel's normal grace.
mkdir churn
for wave in $(jot 60); do
    for n in $(jot 32); do
        printf '%s %s\n' "$wave" "$n" > "churn/$n"
    done
    sync
    rm churn/*
    sync
    sleep 2
    now_scans=$(sysctl -n kern.tessera.gc_scans)
    now_grows=$(sysctl -n kern.tessera.gc_manifest_stack_grows)
    now_reclaimed=$(sysctl -n kern.tessera.gc_reclaimed)
    if [ "$now_scans" -gt "$scans" ] && [ "$now_grows" -gt "$grows" ] &&
        [ "$now_reclaimed" -gt "$reclaimed" ]; then
        break
    fi
done
echo "gc_scans $scans -> $now_scans, gc_manifest_stack_grows $grows -> $now_grows, gc_reclaimed $reclaimed -> $now_reclaimed"
test "$now_scans" -gt "$scans"
test "$now_grows" -gt "$grows"
test "$now_reclaimed" -gt "$reclaimed"

for sample in $(jot 6); do
    for arm in direct scrinium; do
        actual=$(sha256 -q "results/$arm.osheet")
        if [ "$actual" != "$expected" ]; then
            echo "FAIL gc sample=$sample arm=$arm expected=$expected actual=$actual" >&2
            exit 1
        fi
        echo "PASS gc sample=$sample arm=$arm $actual"
    done
done
for arm in direct scrinium; do
    native_memory check dense "results/$arm.osheet" 8
done

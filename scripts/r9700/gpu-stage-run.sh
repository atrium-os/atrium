#!/bin/sh
# gpu-stage-run.sh: one silicon bring-up run of atrium_gpu_amd_pci on the
# R9700 box, done in a single ssh call (docs/spec/r9700-bare-metal.md).
#
#   gpu-stage-run.sh <sha256> <stage>...     stages: 3 4a 4b 4c 4d
#
# The kmod must already be at /root/kmod/atrium_gpu_amd_pci.ko. The script
# checks its sha256, probes before, clears the kernel message buffer, sets
# the kenvs for the stages, loads the module and hands it the GPU (attach
# ends in ENXIO after the mode 1 reset, as designed), saves the kernel log,
# gives the GPU back to vgapci, unsets the kenvs, unloads, probes after, and
# takes one temperature reading 30 s later. The whole report is printed and
# kept in /root/runs/. It never sets keep_state, gfx_cg or mode1_reset.
#
# This loads a kernel module and writes the GPU: only run it with the
# owner's explicit go for that run.
set -u

KO=/root/kmod/atrium_gpu_amd_pci.ko
PROBE=/root/atrium-gpu-probe
GPU_SEL=pci0:3:0:0
GPU_DEV=vgapci0
KNOBS="psp_bootload psp_ipfw smu_init gc_up_ok gmc ih gfx mes"

[ $# -ge 2 ] || { echo "usage: $0 <sha256> <stage>..." >&2; exit 2; }
want=$1; shift

kenvs=""
for s in "$@"; do
	case $s in
	3)  kenvs="$kenvs psp_bootload psp_ipfw smu_init gc_up_ok" ;;
	4a) kenvs="$kenvs gmc" ;;
	4b) kenvs="$kenvs ih" ;;
	4c) kenvs="$kenvs gfx" ;;
	4d) kenvs="$kenvs mes" ;;
	*)  echo "unknown stage $s" >&2; exit 2 ;;
	esac
done

mkdir -p /root/runs
out=/root/runs/$(date -u +%Y%m%dT%H%M%SZ).log

temps() {
	sysctl -a 2>/dev/null | grep -E 'pchtherm.*temperature|acpi\.thermal\.tz[0-9]+\.temperature' |
	    tr '\n' ' '
	echo
}

run() {
echo "== run $(date -u) stages: $* kenvs:$kenvs"
got=$(sha256 -q "$KO" 2>/dev/null)
if [ "$got" != "$want" ]; then
	echo "REFUSED: $KO sha256 $got, want $want"
	exit 1
fi
echo "sha256 ok: $got"
if kldstat -q -m atrium_gpu_amd_pci 2>/dev/null || kldstat | grep -q atrium_gpu_amd; then
	echo "REFUSED: an atrium_gpu_amd module is already loaded"
	exit 1
fi
echo "== before"; temps
[ -x "$PROBE" ] && "$PROBE" pci 3:0:0 --status

sysctl kern.msgbuf_clear=1 >/dev/null
for k in $kenvs; do kenv -q hw.atrium_gpu.$k=1 >/dev/null; done
echo "== kldload"
kldload "$KO"; echo "kldload rc=$?"
devctl set driver -f "$GPU_DEV" atrium_gpu_amd_pci; echo "attach rc=$? (ENXIO expected)"
echo "== kernel log"
dmesg | sed -n '/atrium_gpu_amd/,$p'
echo "stray lines: $(dmesg | grep -ci stray)"

echo "== cleanup"
devctl set driver -f "$GPU_SEL" vgapci; echo "give-back rc=$?"
for k in $KNOBS keep_state gfx_cg mode1_reset; do kenv -q -u hw.atrium_gpu.$k 2>/dev/null; done
echo "kenvs left: $(kenv | grep -c hw.atrium_gpu)"
kldunload atrium_gpu_amd_pci; echo "kldunload rc=$?"
echo "atrium modules loaded: $(kldstat | grep -c atrium_gpu_amd)"

echo "== after"
[ -x "$PROBE" ] && "$PROBE" pci 3:0:0 --status
sleep 30
echo "== temps 30 s after"; temps
echo "== done, log $out"
}

run "$@" 2>&1 | tee "$out"

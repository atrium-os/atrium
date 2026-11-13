#!/bin/sh
# gpu-stage-run.sh: one silicon bring-up run of atrium_gpu_amd_pci on the
# R9700 box, done in a single ssh call (docs/spec/r9700-bare-metal.md).
#
#   gpu-stage-run.sh <sha256> <stage>...     stages: 3 4a 4b 4c 4d 4e 5a 5b 5c1
#   gpu-stage-run.sh <sha256>,<gpu sha256>,<test sha256> 5c2
#   gpu-stage-run.sh <sha256>,<gpu sha256>,<test sha256>,<suite sha256>,<iso sha256> 5f
#   FUZZ_SHA=<sha256> gpu-stage-run.sh <sha256>,<gpu sha256>,<test sha256> fz:<seconds>:<seed>
#
# The kmod must already be at /root/kmod/atrium_gpu_amd_pci.ko. Stage 5c2
# (the client node on silicon) also needs the gpu module at
# /root/kmod/atrium_gpu_amd.ko and the userspace test at /root/si-client-test
# (tests/si_client_test.c), each checked against its sha256: it loads both
# modules, attaches, runs the test once against /dev/atrium-gpu0, then
# detaches and unloads both. Stage 5f is 5c2 plus the two acceptance suites
# on queue fds, /root/atrium-gpu-test (tests/atrium_gpu_test.c) and
# /root/test-gpu-isolation (tests/test_gpu_isolation.c), each run once after
# si-client-test and checked against its sha256. The script
# checks its sha256, probes before, clears the kernel message buffer, sets
# the kenvs for the stages, loads the module and hands it the GPU (attach
# ends in ENXIO after the mode 1 reset, as designed; with stage 5a it stays
# attached for 60 s with telemetry watched, then detaches), saves the kernel log,
# gives the GPU back to vgapci, unsets the kenvs, unloads, probes after, and
# takes one temperature reading 30 s later. Stage fz is 5c2 plus the §10.5
# fuzz gate after si-client-test: /root/si-fuzz (tests/si_fuzz.c, checked
# against FUZZ_SHA) in mode "all" for <seconds> with <seed>, then
# si-client-test once more as the health check. The kernel's messages over
# the whole run are in /var/log/messages (the buffer wraps). The whole report is printed and
# kept in /root/runs/. It never sets keep_state, gfx_cg or mode1_reset.
#
# This loads a kernel module and writes the GPU: only run it with the
# owner's explicit go for that run.
set -u

KO=/root/kmod/atrium_gpu_amd_pci.ko
GPU_KO=/root/kmod/atrium_gpu_amd.ko
TEST=/root/si-client-test
SUITE=/root/atrium-gpu-test
ISO=/root/test-gpu-isolation
PROBE=/root/atrium-gpu-probe
FUZZ=/root/si-fuzz
GPU_SEL=pci0:3:0:0
GPU_DEV=vgapci0
KNOBS="psp_bootload psp_ipfw smu_init gc_up_ok gmc ih gfx mes dispatch attach mes_mscratch vm_test uq_test client"

[ $# -ge 2 ] || { echo "usage: $0 <sha256> <stage>..." >&2; exit 2; }
shas=$1; shift
want=${shas%%,*}
want_gpu=""; want_test=""; want_suite=""; want_iso=""
case $shas in
*,*,*,*,*) rest=${shas#*,}; want_gpu=${rest%%,*}; rest=${rest#*,}
	want_test=${rest%%,*}; rest=${rest#*,}; want_suite=${rest%%,*}
	want_iso=${rest#*,} ;;
*,*,*) rest=${shas#*,}; want_gpu=${rest%%,*}; want_test=${rest#*,} ;;
esac

kenvs=""
hold=0
client=0
suites=0
fuzz=""
for s in "$@"; do
	case $s in
	3)  kenvs="$kenvs psp_bootload psp_ipfw smu_init gc_up_ok" ;;
	4a) kenvs="$kenvs gmc" ;;
	4b) kenvs="$kenvs ih" ;;
	4c) kenvs="$kenvs gfx" ;;
	4d) kenvs="$kenvs mes" ;;
	4e) kenvs="$kenvs dispatch" ;;	# needs 4d
	5a) kenvs="$kenvs psp_bootload psp_ipfw gc_up_ok attach"; hold=60 ;;
	5b) kenvs="$kenvs psp_bootload psp_ipfw gc_up_ok attach vm_test"; hold=30 ;;
	5c1) kenvs="$kenvs psp_bootload psp_ipfw gc_up_ok attach vm_test uq_test"; hold=30 ;;
	5c2) kenvs="$kenvs psp_bootload psp_ipfw gc_up_ok attach client"; hold=10; client=1 ;;
	5f) kenvs="$kenvs psp_bootload psp_ipfw gc_up_ok attach client"; hold=10; client=1; suites=1 ;;
	fz:*:*) kenvs="$kenvs psp_bootload psp_ipfw gc_up_ok attach client"; hold=10; client=1; fuzz=${s#fz:} ;;
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
if [ "$client" = 1 ]; then
	g=$(sha256 -q "$GPU_KO" 2>/dev/null); t=$(sha256 -q "$TEST" 2>/dev/null)
	if [ -z "$want_gpu" ] || [ "$g" != "$want_gpu" ] || [ "$t" != "$want_test" ]; then
		echo "REFUSED: $GPU_KO sha256 $g (want $want_gpu), $TEST sha256 $t (want $want_test)"
		exit 1
	fi
	echo "sha256 ok: gpu module $g, test $t"
fi
if [ "$suites" = 1 ]; then
	u=$(sha256 -q "$SUITE" 2>/dev/null); i=$(sha256 -q "$ISO" 2>/dev/null)
	if [ -z "$want_iso" ] || [ "$u" != "$want_suite" ] || [ "$i" != "$want_iso" ]; then
		echo "REFUSED: $SUITE sha256 $u (want $want_suite), $ISO sha256 $i (want $want_iso)"
		exit 1
	fi
	echo "sha256 ok: suite $u, isolation $i"
fi
if [ -n "$fuzz" ]; then
	f=$(sha256 -q "$FUZZ" 2>/dev/null)
	if [ -z "${FUZZ_SHA:-}" ] || [ "$f" != "$FUZZ_SHA" ]; then
		echo "REFUSED: $FUZZ sha256 $f (want ${FUZZ_SHA:-unset})"
		exit 1
	fi
	echo "sha256 ok: si-fuzz $f"
fi
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
[ "$client" = 1 ] && { kldload "$GPU_KO"; echo "kldload gpu module rc=$?"; }
if [ "$hold" -gt 0 ]; then
	devctl set driver -f "$GPU_DEV" atrium_gpu_amd_pci; echo "attach rc=$? (0 expected: stays attached)"
	if [ "$client" = 1 ]; then
		ls -l /dev/atrium-gpu0 /dev/atrium-gpu0ctl
		echo "== si-client-test"
		timeout 600 "$TEST"; echo "si-client-test rc=$?"
		if [ "$suites" = 1 ]; then
			echo "== atrium-gpu-test"
			timeout 300 "$SUITE"; echo "atrium-gpu-test rc=$?"
			echo "== test-gpu-isolation"
			timeout 600 "$ISO"; echo "test-gpu-isolation rc=$?"
			sysctl dev.atrium_gpu_amd_pci.0.vram 2>/dev/null
		fi
		if [ -n "$fuzz" ]; then
			fsecs=${fuzz%%:*}; fseed=${fuzz#*:}
			r0=$(sysctl -n dev.atrium_gpu_amd_pci.0.si_protected_regs 2>/dev/null | awk '/^reinits/ {print $2}')
			echo "== si-fuzz all $fsecs $fseed"
			timeout $((fsecs + 1800)) "$FUZZ" all "$fsecs" "$fseed"; echo "si-fuzz rc=$?"
			sysctl dev.atrium_gpu_amd_pci.0.si_protected_regs 2>/dev/null | grep reinits
			r1=$(sysctl -n dev.atrium_gpu_amd_pci.0.si_protected_regs 2>/dev/null | awk '/^reinits/ {print $2}')
			# si-client-test 14 spends 2 of the 3 re-inits per 600 s: let
			# any window the fuzz opened run out first.
			if [ "${r1:-0}" != "${r0:-0}" ]; then
				echo "== re-inits during the fuzz (${r0:-0} -> ${r1:-0}): waiting 610 s"
				sleep 610
			fi
			echo "== si-client-test after the fuzz"
			timeout 600 "$TEST"; echo "si-client-test rc=$?"
		fi
		sysctl dev.atrium_gpu_amd.0.isolation 2>/dev/null
	fi
	echo "== attached, holding ${hold} s"; temps
	sleep "$hold"
	echo "== after ${hold} s attached"; temps
	# Deferred BO frees can hold the gpu child for a moment (EBUSY).
	for i in 1 2 3 4 5; do
		devctl set driver -f "$GPU_SEL" vgapci; rc=$?
		echo "detach + give-back rc=$rc (try $i)"
		[ "$rc" = 0 ] && break
		sleep 2
	done
else
	devctl set driver -f "$GPU_DEV" atrium_gpu_amd_pci; echo "attach rc=$? (ENXIO expected)"
fi
echo "== kernel log"
dmesg | sed -n '/atrium_gpu_amd/,$p'
echo "stray lines: $(dmesg | grep -ci stray)"

echo "== cleanup"
devctl set driver -f "$GPU_SEL" vgapci; echo "give-back rc=$?"
for k in $KNOBS keep_state gfx_cg mode1_reset; do kenv -q -u hw.atrium_gpu.$k 2>/dev/null; done
echo "kenvs left: $(kenv | grep -c hw.atrium_gpu)"
[ "$client" = 1 ] && { kldunload atrium_gpu_amd; echo "kldunload gpu module rc=$?"; }
kldunload atrium_gpu_amd_pci; echo "kldunload rc=$?"
echo "atrium modules loaded: $(kldstat | grep -c atrium_gpu_amd)"

echo "== after"
[ -x "$PROBE" ] && "$PROBE" pci 3:0:0 --status
sleep 30
echo "== temps 30 s after"; temps
echo "== done, log $out"
}

run "$@" 2>&1 | tee "$out"

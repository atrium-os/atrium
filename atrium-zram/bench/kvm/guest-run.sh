#!/bin/sh
# Runs in the guest on the patched kernel. For each codec: a dd throughput
# check, then the 1.9 GB checksummed fill with a dtrace kernel profile, the
# in-kernel timing counters and the re-compression bench, once with the swap
# fast path off (pages go through GEOM) and once with it on (spec §6.1, design
# A). Results go to /root/zb/results; the summary to stdout.
set -u
. /root/zb/env
R=/root/zb/results
mkdir -p "$R"
cd /root/zb
KO=/root/zb/zram/atrium_zram.ko
log() { echo "[guest-run $(date +%H:%M:%S)] $*"; }
unl() { swapoff /dev/zram0 2>/dev/null; kldunload atrium_zram 2>/dev/null; true; }
t() { /usr/bin/time -p sh -c "$1" 2>&1 | awk '/real/ { print $2 }'; }

{ uname -a; sysctl hw.model hw.ncpu hw.physmem vm.swap_free_notify; } > "$R/system.txt" 2>&1
[ -n "${SKIP_PATCH_CHECK:-}" ] || sysctl -n vm.stats.swap.fast_hits >/dev/null 2>&1 ||
	{ echo "patched kernel (with the swap fast path) not running"; exit 1; }
RUNS="${RUNS:-zstd:0 zstd:1 lz4:0 lz4:1}"	# codec:fastpath
FILL_MB="${FILL_MB:-1900}"; FILL_SECS="${FILL_SECS:-180}"
swapoff -a >/dev/null 2>&1 || true
kldload dtraceall 2>/dev/null || true
unl
kldload "$KO" || exit 1
sysctl -n kern.zram.selftest > "$R/selftest.txt"
unl
find /usr/share /usr/include -type f -size -200k | xargs cat 2>/dev/null | head -c 33554432 > /root/src32
find /usr/bin /usr/lib /usr/libexec /usr/share /boot/kernel -type f > /root/filelist

for C in zstd lz4; do
	log "$C: dd"
	kldload "$KO"; sysctl -q kern.zram.mem_limit=0 kern.zram.codec=$C
	cat /root/src32 > /dev/null
	{
		echo "1x write 32 MiB: $(t 'dd if=/root/src32 of=/dev/zram0 bs=64k 2>/dev/null') s"
		echo "1x read  32 MiB: $(t 'dd if=/dev/zram0 of=/dev/null bs=64k count=512 2>/dev/null') s"
		echo "4x write 128 MiB: $(t 'for i in 0 1 2 3; do dd if=/root/src32 of=/dev/zram0 bs=64k oseek=$((i*512+1024)) 2>/dev/null & done; wait') s"
		echo "4x read  128 MiB: $(t 'for i in 0 1 2 3; do dd if=/dev/zram0 of=/dev/null bs=64k iseek=$((i*512+1024)) count=512 2>/dev/null & done; wait') s"
		ok=0; for i in 0 1 2 3; do dd if=/dev/zram0 bs=64k iseek=$((i*512+1024)) count=512 2>/dev/null | cmp -s - /root/src32 && ok=$((ok+1)); done
		echo "round-trip streams OK: $ok/4"
	} > "$R/dd_$C.txt"
	unl
done

for RN in $RUNS; do
	C=${RN%:*}; F=${RN#*:}; T=${C}_f$F
	log "$T: fill $FILL_MB MB, $FILL_SECS s, fast path $F, with dtrace profile"
	kldload "$KO"; sysctl -q kern.zram.mem_limit=0 kern.zram.codec=$C kern.zram.fastpath=$F
	swapon /dev/zram0
	fh0=$(sysctl -n vm.stats.swap.fast_hits 2>/dev/null || echo 0)
	out0=$(sysctl -n vm.stats.vm.v_swappgsout); in0=$(sysctl -n vm.stats.vm.v_swappgsin)
	dtrace -q -x stackframes=16 -x aggsize=16m \
	    -n 'profile-997 /arg0 != 0/ { @[stack(16)] = count(); }' -o "$R/prof_$T.txt" &
	D=$!
	( while :; do echo "$(date +%s) $(sysctl -n kern.zram.orig_bytes kern.zram.pool_bytes kern.zram.pages_comp kern.zram.pages_raw kern.zram.pages_same kern.zram.fail_nomem | tr '\n' ' ')"; sleep 5; done ) > "$R/samples_$T.txt" &
	S=$!
	( sleep "${BENCH_AT:-120}"; sysctl -n kern.zram.bench > "$R/bench_$T.txt" ) &
	B=$!
	/usr/bin/time -l ./fill "$FILL_MB" "$FILL_SECS" < /root/filelist > "$R/fill_$T.txt" 2>&1
	kill -TERM $D; wait $D 2>/dev/null	# a background job ignores SIGINT; dtrace prints on TERM
	kill $S 2>/dev/null; wait $B 2>/dev/null
	sysctl -n kern.zram.timing_stats > "$R/timing_$T.txt"
	echo "swap pages out $(( $(sysctl -n vm.stats.vm.v_swappgsout) - out0 )), in $(( $(sysctl -n vm.stats.vm.v_swappgsin) - in0 )), fast-path requests $(( $(sysctl -n vm.stats.swap.fast_hits 2>/dev/null || echo 0) - fh0 )) (active $(sysctl -n kern.zram.fastpath_active 2>/dev/null))" > "$R/vmstat_$T.txt"
	sleep 30
	{ sysctl kern.zram; swapinfo; } > "$R/after_$T.txt" 2>&1
	awk -f /root/zb/classify.awk "$R/prof_$T.txt" > "$R/classify_$T.txt"
	unl
done

echo "===ZRAM-BENCH-REPORT BEGIN==="
cat "$R/system.txt"
echo "selftest: $(cat "$R/selftest.txt")"
for C in zstd lz4; do
	echo "--- dd $C"
	cat "$R/dd_$C.txt"
done
for RN in $RUNS; do
	C=${RN%:*}; F=${RN#*:}; T=${C}_f$F
	echo "--- $C, fast path $F"
	grep -E 'corrupt|real' "$R/fill_$T.txt" | head -3
	cat "$R/vmstat_$T.txt"
	echo "peak (orig pool comp raw same nomem): $(sort -k2 -n "$R/samples_$T.txt" | tail -1 | cut -d' ' -f2-)"
	echo "timing: $(cat "$R/timing_$T.txt")"
	echo "bench: $(cat "$R/bench_$T.txt" 2>/dev/null)"
	cat "$R/classify_$T.txt"
	grep -E 'fail_|fast_|orig_bytes' "$R/after_$T.txt" | tr '\n' ' '; echo
done
echo "===ZRAM-BENCH-REPORT END==="

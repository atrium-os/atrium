#!/bin/sh
# fresco-cpu.sh — CPU seconds the Fresco processes use over a window, run
# inside the VM (or on hardware) while a client animates.
#
#   sh scripts/fresco-cpu.sh [SECONDS]     (default 20; needs root)
#
# For atrium-frescod it also prints the part spent inside the GPU submit
# ioctl's doorbell write. On gpusim that write runs the submitted work (the
# scanout upload's DMA) synchronously inside the MMIO exit, so the guest
# charges the emulated GPU's time to frescod; on hardware the write is
# posted and costs next to nothing. "own" is frescod's time without it,
# the number to compare between builds. Measured with DTrace (fbt on
# atrium_gpu_amd): amd_hwq_submit's time less the functions it calls,
# which leaves the doorbell write and a few locks.
#
# Prints one line per process name:
#   atrium-frescod 19.27 own 11.60 emulated-gpu 7.67
#   fresco-renderer 13.57
set -eu
SECS=${1:-20}

kldstat -q -m dtraceall 2>/dev/null || kldload dtraceall

# CPU seconds per process name, from ps (all processes of that name).
cpu() {
	ps -axo comm,time | awk '
	{ n = split($2, a, ":"); s = (n == 2) ? a[1] * 60 + a[2] : a[1] * 3600 + a[2] * 60 + a[3]; c[$1] += s }
	END { for (k in c) if (k ~ /fresc|render/) printf "%s %.2f\n", k, c[k] }' | sort
}

T=$(mktemp -d /tmp/fresco-cpu.XXXXXX)
trap 'rm -rf "$T"' EXIT

# Time inside amd_hwq_submit, less its callees, for frescod's threads:
# a running total, printed each second with the wall clock.
dtrace -q -n '
self uint64_t s, k, kids;
self int d;
BEGIN { @ns = sum(0); }
fbt:atrium_gpu_amd:amd_hwq_submit:entry /execname == "atrium-frescod"/ {
	self->s = vtimestamp; self->kids = 0; self->d = 0;
}
fbt:atrium_gpu_amd::entry /self->s && probefunc != "amd_hwq_submit"/ {
	self->k = self->d == 0 ? vtimestamp : self->k; self->d++;
}
fbt:atrium_gpu_amd::return /self->s && probefunc != "amd_hwq_submit" && self->d > 0/ {
	self->d--; self->kids += self->d == 0 ? vtimestamp - self->k : 0;
}
fbt:atrium_gpu_amd:amd_hwq_submit:return /self->s/ {
	@ns = sum(vtimestamp - self->s - self->kids); self->s = 0;
}
tick-1s { printf("%d ", walltimestamp / 1000000000); printa("%@d\n", @ns); }' > "$T/ns" 2>"$T/err" &
D=$!
# The total at wall second $1 (the first line at or after it).
emu_at() {
	until awk -v t="$1" '$1 >= t { found = 1 } END { exit !found }' "$T/ns" 2>/dev/null; do
		kill -0 $D 2>/dev/null || { cat "$T/err" >&2; exit 1; }
		sleep 0.2
	done
	awk -v t="$1" '$1 >= t { print $2; exit }' "$T/ns"
}
emu_at 0 >/dev/null	# DTrace is attached
t0=$(date +%s); cpu > "$T/c0"
sleep "$SECS"
t1=$(date +%s); cpu > "$T/c1"
E0=$(emu_at "$t0"); E1=$(emu_at "$t1")
kill $D 2>/dev/null; wait $D 2>/dev/null || true
NS=$((E1 - E0))
join "$T/c0" "$T/c1" | awk -v ns="$NS" '{
	d = $3 - $2
	if ($1 == "atrium-frescod") {
		g = ns / 1e9
		printf "%s %.2f own %.2f emulated-gpu %.2f\n", $1, d, d - g, g
	} else
		printf "%s %.2f\n", $1, d
}'

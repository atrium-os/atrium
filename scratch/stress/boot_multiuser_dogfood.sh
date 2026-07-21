#!/bin/sh
# Dogfood the full-userland Tessera root: boot multiuser, then drive a realistic
# mixed workload on the LIVE root (compile many C files + link + run; tar/untar
# a chunk of the userland with byte-verify; tree-walk/du/grep; churn) for a few
# rounds, then report failures + dmesg errors and leave the guest for a
# crash-kill fsck. Dev VM must be down.
set -u
BSD_DIR="$(cd "$(dirname "$0")/../.." && pwd)"
QEMU_DIR="$BSD_DIR/external/qemu-build"
QEMU="$QEMU_DIR/build/qemu-system-aarch64"
EFI_PAD="$BSD_DIR/vm/edk2-aarch64-code.fd"
VARS="$BSD_DIR/vm/edk2-vars-mu.fd"
ROOT="$BSD_DIR/vm/tessera-root.img"
LOG="${1:-/tmp/mu-dogfood.log}"
ROUNDS="${ROUNDS:-4}"
FIFO="/tmp/mu-dogfood.in"

cp "$BSD_DIR/vm/edk2-vars-blank.fd" "$VARS"
rm -f "$FIFO"; mkfifo "$FIFO"; : > "$LOG"

"$QEMU" -L "$QEMU_DIR/pc-bios" \
    -accel hvf -cpu host -machine virt,gic-version=3 -smp 4 -m 4096 \
    -drive if=pflash,format=raw,unit=0,file="$EFI_PAD",readonly=on \
    -drive if=pflash,format=raw,unit=1,file="$VARS" \
    -drive file="$ROOT",format=raw,cache=directsync,if=none,id=troot \
    -device virtio-blk-pci,drive=troot,serial=tessera-root,config-wce=on \
    -nographic -serial mon:stdio <"$FIFO" >"$LOG" 2>&1 &
QPID=$!
exec 3>"$FIFO"

ok=0
for i in $(seq 1 90); do
    grep -q "login:" "$LOG" && { ok=1; break; }
    grep -qiE "panic|mountroot>|Enter full pathname" "$LOG" && break
    sleep 1
done
if [ "$ok" = "1" ]; then
    sleep 1; printf 'root\n' >&3; sleep 2
    # write the dogfood workload to the root, then run it
    printf "cat > /root/dogfood.sh <<'ZZ'\n" >&3
    printf '%s\n' \
'set -u' \
'fail=0; r=0' \
'while [ $r -lt '"$ROUNDS"' ]; do' \
'  echo "=== round $r ==="' \
'  # 1. build: generate + compile + link many C files, run the result' \
'  rm -rf /root/build; mkdir -p /root/build; cd /root/build' \
'  i=0; while [ $i -lt 40 ]; do' \
'    printf "int f%d(int x){return x*%d+%d;}\\n" $i $i $r > m$i.c' \
'    cc -O2 -c m$i.c -o m$i.o 2>>/root/build.err || { echo "CC FAIL m$i"; fail=1; }' \
'    i=$((i+1))' \
'  done' \
'  printf "extern int f0(int);int main(){return f0(1)&0;}\\n" > main.c' \
'  cc -O2 main.c m*.o -o prog 2>>/root/build.err || { echo "LINK FAIL"; fail=1; }' \
'  ./prog; echo "prog rc=$?"' \
'  cd /; rm -rf /root/build' \
'  # 2. archive: tar a chunk of the userland, extract, byte-verify a sample' \
'  tar cf /root/u.tar /usr/bin /usr/lib 2>/dev/null || { echo "TAR FAIL"; fail=1; }' \
'  rm -rf /root/ex; mkdir -p /root/ex; tar xf /root/u.tar -C /root/ex 2>/dev/null || { echo "UNTAR FAIL"; fail=1; }' \
'  cmp /usr/bin/cc /root/ex/usr/bin/cc && echo "cmp cc: ok" || { echo "CMP MISMATCH"; fail=1; }' \
'  cmp /usr/lib/libc++.so.1 /root/ex/usr/lib/libc++.so.1 && echo "cmp libc++: ok" || { echo "CMP MISMATCH libc++"; fail=1; }' \
'  rm -rf /root/ex /root/u.tar' \
'  # 3. metadata-heavy: tree walk, disk usage, recursive grep' \
'  echo "files: $(find / -type f 2>/dev/null | wc -l | tr -d \" \")"' \
'  du -sh /usr >/dev/null 2>&1 || fail=1' \
'  grep -rl root /etc >/dev/null 2>&1' \
'  sync' \
'  r=$((r+1))' \
'done' \
'echo "DOGFOOD_FAIL=$fail"' \
'echo DOGFOOD_DONE' >&3
    printf 'ZZ\n' >&3
    sleep 1
    printf 'sh /root/dogfood.sh\n' >&3
    # wait for the workload to finish (long — real compiles + tar of /usr)
    for i in $(seq 1 600); do
        grep -q "^DOGFOOD_DONE" "$LOG" && break
        grep -qiE "panic" "$LOG" && break
        sleep 2
    done
    sleep 1
    # report
    printf 'echo BUILD_ERRS=$(wc -l < /root/build.err 2>/dev/null || echo 0)\n' >&3
    printf 'mount | grep " / "\n' >&3
    printf 'df -h / | tail -1\n' >&3
    printf 'echo COMMITS=$(sysctl -n kern.tessera.sb_commits)\n' >&3
    printf 'echo BUMP=$(sysctl -n kern.tessera.meta_exhaust) EXHAUST\n' >&3
    printf 'echo TESSCAS_WITNESS=$(dmesg | grep -c "tess_cas")\n' >&3
    printf 'echo REPORT_DONE\n' >&3
    for i in $(seq 1 30); do grep -q "^REPORT_DONE" "$LOG" && break; sleep 1; done
    sleep 1
fi
kill "$QPID" 2>/dev/null || true
exec 3>&- ; rm -f "$FIFO"

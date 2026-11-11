#!/bin/sh
# Kernel timing of tessera_fs.ko builds against each other (D3,
# atrium-tessera/docs/rust-decoders.md): the all-C decoders, the Rust ones,
# and the Rust ones with cross-language LTO. Meant for a guest with real
# timing (HVF on the Mac); under TCG the numbers mean little.
#
#   MODULES="tessera_fs-c.ko tessera_fs.ko tessera_fs-lto.ko" \
#       sh scripts/tessera-decode-vm-e2e.sh deploy      # modules + tools
#   sh scripts/tessera-decode-timing.sh [rounds]        # default 11
#
# One image is built once (with the first module), then every round loads
# each module in turn (order rotated per round), mounts the image fresh, and
# times three read-only passes that go through the decoders:
#   stat  ls -l of a directory of $NFILES files (dir B-tree, inode manifests)
#   small cat of every small file (manifest + chunk lookups)
#   blob  read of a $BLOB_MB MiB file (pack index, extents, BLAKE3 verify)
# /usr/bin/time has 10 ms resolution, so the defaults (20000 files, 256 MiB)
# are sized for HVF; for a TCG dry run use NFILES=300 BLOB_MB=8 and 1-2 rounds.
# The module is unloaded between rounds, so nothing is cached in tessera_fs.
# Prints the median of each pass per module and the ratio to the first.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VSSH="${VSSH:-$ROOT/scripts/vssh}"
MODULES="${MODULES:-tessera_fs-c.ko tessera_fs.ko tessera_fs-lto.ko}"
ROUNDS="${1:-11}"
NFILES="${NFILES:-20000}"
BLOB_MB="${BLOB_MB:-256}"

guest_script() {
    cat <<'EOF'
set -u
R=$1; N=$2; BMB=$3; shift 3; MODS="$*"
T=/root/d3; M=/mnt/d3t; IMG=/var/tmp/d3t.img
PATH=$T:$PATH
fail() { echo "FAIL: $*"; exit 1; }
unload() { for id in $(kldstat | awk '/tessera_fs/ {print $1}'); do kldunload -i $id 2>/dev/null; done; }
secs() { /usr/bin/time -p sh -c "$1" 2>&1 >/dev/null | awk '/^real/ {print $2}'; }
umount $M 2>/dev/null; unload
for u in $(mdconfig -l); do mdconfig -lv -u $u | grep -q d3t.img && mdconfig -d -u $u; done
set -- $MODS
kldload $T/$1 || fail "kldload $1"
rm -f $IMG; truncate -s $((BMB * 2 + 512))m $IMG
DEV=/dev/$(mdconfig -a -t vnode -f $IMG) || fail mdconfig
mkfs-tessera $DEV >/dev/null 2>&1 || fail mkfs
mkdir -p $M; mount -t tessera $DEV $M || fail mount
mkdir $M/d; i=0
while [ $i -lt $N ]; do echo "file $i" > $M/d/f$i; i=$((i+1)); done
dd if=/dev/random of=$M/blob bs=1m count=$BMB 2>/dev/null
umount $M || fail umount; unload
r=0
while [ $r -lt $R ]; do
    # rotate the order each round
    k=$((r % $#)); set -- $MODS; j=0
    while [ $j -lt $k ]; do x=$1; shift; set -- "$@" $x; j=$((j+1)); done
    for ko in "$@"; do
        kldload $T/$ko || fail "kldload $ko"
        mount -t tessera $DEV $M || fail "mount $ko"
        s=$(secs "ls -l $M/d")
        f=$(secs "cat $M/d/* > /dev/null")
        b=$(secs "cat $M/blob > /dev/null")
        umount $M || fail "umount $ko"; unload
        echo "R $r $ko $s $f $b"
    done
    r=$((r+1))
done
mdconfig -d -u ${DEV#/dev/md}; rm -f $IMG
echo DONE
EOF
}

log=/root/d3/timing.log
guest_script | "$VSSH" 'cat > /root/d3/timing.sh'
"$VSSH" "rm -f $log; nohup sh /root/d3/timing.sh $ROUNDS $NFILES $BLOB_MB $MODULES > $log 2>&1 < /dev/null &"
n=0
while :; do
    sleep 20
    out=$("$VSSH" "cat $log" 2>/dev/null | tr -d '\r') || { n=$((n+1)); continue; }
    echo "$out" | grep -qE '^(DONE|FAIL)' && break
    n=$((n+1)); [ $n -lt "${POLL_MAX:-360}" ] || { echo "no result after polling" >&2; exit 1; }
done
echo "$out" | grep -q '^FAIL' && { echo "$out" | grep '^FAIL' >&2; exit 1; }
echo "$out" | awk -v mods="$MODULES" '
    /^R / { s[$3] = s[$3] " " $4; f[$3] = f[$3] " " $5; b[$3] = b[$3] " " $6 }
    function med(list,   a, n, i, j, t) {
        n = split(list, a, " ")
        for (i = 2; i <= n; i++) for (j = i; j > 1 && a[j-1] + 0 > a[j] + 0; j--) { t = a[j]; a[j] = a[j-1]; a[j-1] = t }
        return (n % 2) ? a[(n+1)/2] : (a[n/2] + a[n/2+1]) / 2
    }
    END {
        nm = split(mods, m, " ")
        printf "%-20s %8s %8s %8s   (median seconds; ratio to %s)\n", "module", "stat", "small", "blob", m[1]
        for (i = 1; i <= nm; i++) {
            S[i] = med(s[m[i]]); F[i] = med(f[m[i]]); B[i] = med(b[m[i]])
            printf "%-20s %8.3f %8.3f %8.3f   %.3f %.3f %.3f\n", m[i], S[i], F[i], B[i],
                S[1] ? S[i]/S[1] : 0, F[1] ? F[i]/F[1] : 0, B[1] ? B[i]/B[1] : 0
        }
    }'

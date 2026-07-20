#!/bin/sh
# Focused POSIX rename behavior probe (run in the VM). Exercises the cases the
# old "v1" comment claimed were unsupported (cross-dir, overwrite) plus the
# known-weak spots: cross-dir directory move (.. must resolve to the NEW
# parent) and the deep subtree-loop check (rename a dir into its own
# descendant must fail EINVAL, not just at depth 1).
set -u
BIN=/mnt/host/atrium-tessera/rs/target/aarch64-unknown-freebsd/release
DEV=/dev/md9; IMG=/tmp/rename.img; MNT=/mnt/rn
umount $MNT 2>/dev/null; mdconfig -d -u 9 2>/dev/null
kldunload tessera_fs 2>/dev/null; kldload /mnt/host/atrium-tessera/kmod/tessera_fs.ko
rm -f $IMG; $BIN/mkfs-tessera --create -s 64 $IMG >/dev/null
mdconfig -a -t vnode -u 9 -f $IMG >/dev/null
# direct rename(2) helper — BSD mv moves INTO an existing dir, masking the
# rename-overwrite semantics we need to test.
cat > /tmp/ren.c <<'EOF'
#include <stdio.h>
#include <string.h>
#include <errno.h>
int main(int c,char**v){ if(c!=3){return 2;} if(rename(v[1],v[2])!=0){printf("errno=%s\n",strerror(errno));return 1;} printf("ok\n"); return 0;}
EOF
cc -O2 -o /tmp/ren /tmp/ren.c
mkdir -p $MNT; mount -t tessera $DEV $MNT
cd $MNT

pass=0; fail=0
ok(){ echo "  ok: $1"; pass=$((pass+1)); }
no(){ echo "  FAIL: $1"; fail=$((fail+1)); }

echo "=== 1. same-dir file rename ==="
echo aaa > f1; mv f1 f2
[ ! -e f1 ] && [ "$(cat f2)" = aaa ] && ok "f1->f2" || no "f1->f2"

echo "=== 2. cross-dir file rename ==="
mkdir d1 d2; echo bbb > d1/g1; mv d1/g1 d2/g2
[ ! -e d1/g1 ] && [ "$(cat d2/g2)" = bbb ] && ok "d1/g1->d2/g2" || no "d1/g1->d2/g2"

echo "=== 3. overwrite file rename (target exists) ==="
echo old > t1; echo new > t2; mv t2 t1
[ ! -e t2 ] && [ "$(cat t1)" = new ] && ok "t2->t1 overwrite" || no "t2->t1 overwrite ($(cat t1 2>&1))"

echo "=== 4. cross-dir DIRECTORY move + .. resolves to NEW parent ==="
mkdir -p src/sub; mkdir dst; mv src/sub dst/sub
# dst/sub/.. should now be dst
ddev=$(stat -f %i dst 2>/dev/null); pdev=$(stat -f %i dst/sub/.. 2>/dev/null)
[ ! -e src/sub ] && [ -d dst/sub ] && ok "dir moved" || no "dir moved (src/sub still there?)"
[ -n "$ddev" ] && [ "$ddev" = "$pdev" ] && ok ".. -> new parent (dst ino=$ddev == ..=$pdev)" || no ".. parent (dst=$ddev ..=$pdev)"

echo "=== 5. rename(2) dir onto EMPTY dir (must succeed, replace) ==="
mkdir e_src e_dst; echo x > e_src/inside
/tmp/ren e_src e_dst >/tmp/e5 2>&1 && \
  { [ ! -e e_src ] && [ -e e_dst/inside ] && ok "dir replaced empty dir" || no "moved but content missing ($(ls e_dst))"; } \
  || no "rename dir->empty dir failed ($(cat /tmp/e5))"

echo "=== 6. rename(2) dir onto NON-empty dir (must fail ENOTEMPTY) ==="
mkdir n_src n_dst; echo x > n_dst/keep
/tmp/ren n_src n_dst >/tmp/e6 2>&1 && no "should have failed (non-empty target)" || ok "rejected: $(cat /tmp/e6)"

echo "=== 7. rename dir into its OWN subtree (must fail EINVAL) ==="
mkdir -p loop/a/b/c
mv loop loop/a/b/c/loop2 2>/tmp/e7 && no "LOOP allowed — created disconnected cycle!" || ok "rejected: $(cat /tmp/e7)"
# depth-1 variant
mkdir -p loop1; mv loop1 loop1/child 2>/tmp/e7b && no "depth-1 loop allowed" || ok "depth-1 rejected: $(cat /tmp/e7b)"

echo "=== 8. file onto dir / dir onto file (type mismatch) ==="
echo x > tf; mkdir td
mv tf td 2>/dev/null; [ -e td/tf ] && ok "file into dir (mv semantics)" || no "file into dir"
mkdir td2; echo x > tf2; mv td2 tf2 2>/tmp/e8 && no "dir onto file allowed" || ok "dir onto file rejected: $(cat /tmp/e8)"

cd /; sync; umount $MNT; mdconfig -d -u 9
echo "=== fsck ==="; $BIN/tessera-fsck $IMG 2>&1 | tail -3
echo "=== RENAME PROBE: $pass ok / $fail FAIL ==="

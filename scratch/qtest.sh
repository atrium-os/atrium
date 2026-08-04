#!/bin/sh
# Tessera per-directory quota exercise (docs/spec/tessera-quotas.md).
# NO set -e: a failing probe must not abort the run — we want every result.
Q=/var/lib/atrium/qtest
LIM=10485760          # 10 MiB
rm -rf $Q; mkdir -p $Q

echo "=== 1. set a ${LIM}-byte quota on $Q ==="
/root/tquota set $Q $LIM

echo "=== 2. statfs INSIDE the domain (spec 3.6: must be quota-scoped, not pool) ==="
/root/tquota statfs $Q
echo "    for comparison, the pool root:"
/root/tquota statfs /

echo "=== 3. write 1 MiB chunks until refused ==="
i=0; written=0
while [ $i -lt 20 ]; do
    if dd if=/dev/random of=$Q/f$i bs=1m count=1 status=none 2>/dev/null; then
        written=$((written + 1)); i=$((i + 1))
    else
        echo "    refused at file #$i after ${written} MiB written"
        break
    fi
done
[ $i -ge 20 ] && echo "    !! wrote 20 MiB into a 10 MiB quota — NOT ENFORCED"

echo "=== 4. what errno? (direct write, no dd) ==="
cat /dev/random | head -c 2097152 > $Q/overflow 2>&1 || echo "    write failed as expected"
ls -l $Q/overflow 2>/dev/null | awk '{print "    overflow file size:", $5}'

echo "=== 5. usage now ==="
du -sk $Q 2>/dev/null | awk '{print "    du:", $1, "KiB"}'
/root/tquota statfs $Q

echo "=== 6. subdirectory inheritance (spec: children inherit the domain) ==="
mkdir -p $Q/sub
dd if=/dev/random of=$Q/sub/deep bs=1m count=5 status=none 2>/dev/null \
  && echo "    !! 5 MiB into a subdir of a FULL domain SUCCEEDED — inheritance broken" \
  || echo "    subdir write refused too — domain inherited"

echo "=== 7. free space, then confirm writes work again ==="
rm -f $Q/f0 $Q/f1 $Q/f2
dd if=/dev/random of=$Q/after-free bs=1m count=2 status=none 2>/dev/null \
  && echo "    2 MiB after freeing 3 MiB: OK (quota released)" \
  || echo "    !! still refused after freeing — quota not released"

echo "=== 8. clear the quota, verify unlimited again ==="
/root/tquota clear $Q
dd if=/dev/random of=$Q/unlimited bs=1m count=15 status=none 2>/dev/null \
  && echo "    15 MiB after clearing: OK" \
  || echo "    !! still refused after clearing"

echo "=== 9. counters ==="
sysctl -n kern.tessera.quota_domains kern.tessera.quota_reject 2>/dev/null
echo QTEST_DONE

#!/bin/sh
# Source-level invariants for the Tessera kmod that no compiler enforces.
# Run from anywhere; exits non-zero on any violation. Add a case, not a
# comment, when a new rule appears — a rule that lives only in prose is a rule
# the next edit does not know about.
set -u
HERE="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$HERE/kmod/tessera_fs.c"
fail=0

# 1. manifest_hash is written ONLY through tessera_fs_ino_set_mft().
#    The GC liveness walk trusts TESSERA_INODE_FLAG_MFT_LEAF to skip fetching
#    a manifest; the helper is what keeps that flag consistent with the hash
#    beside it. A raw memcpy can leave the flag set on a CHUNK_LIST, the walk
#    then skips that file's chunk hashes, and a dedup'd chunk in another pack
#    is freed. (The walk's own push into _stack[], the node cache tn->mft_hash,
#    replay's whole-record restore and copy_file_range's flag-carrying copy are
#    the allowed exceptions, matched below.)
raw=$(grep -nE 'memcpy\([A-Za-z_>.-]*manifest_hash,' "$SRC" \
      | grep -vE '_stack\[|tn->mft_hash|dino\.manifest_hash, sino\.manifest_hash|fake\.manifest_hash|tessera_fs_ino_set_mft|r->manifest_hash, h, TESSERA_HASH_SIZE')
if [ -n "$raw" ]; then
	echo "VIOLATION: raw write to manifest_hash outside tessera_fs_ino_set_mft():"
	echo "$raw" | sed 's/^/  /'
	fail=1
fi

# 2. Only leaf kinds may SET the flag — the helper is the single place.
n=$(grep -c 'flags |= TESSERA_INODE_FLAG_MFT_LEAF' "$SRC")
if [ "$n" -ne 1 ]; then
	echo "VIOLATION: TESSERA_INODE_FLAG_MFT_LEAF is set in $n places (expected 1, inside tessera_fs_ino_set_mft)"
	fail=1
fi

[ $fail -eq 0 ] && echo "tessera invariants: OK"
exit $fail

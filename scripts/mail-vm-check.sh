#!/bin/sh
# Mail against FreeBSD's own Dovecot and Pigeonhole in the dev VM
# (docs/spec/mail.md §16): mail-engine's Dovecot tests, cross-built here and
# run on the guest, so the parse jail (Capsicum), the sync worker, the Arca
# courier, IMAP, submission and ManageSieve all run on FreeBSD for real.
#
#   sh scripts/run-vm.sh            # (Mac) or scripts/cloud-vm.sh up
#   sh scripts/mail-vm-check.sh
#
# The guest needs `pkg install dovecot dovecot-pigeonhole` (done here when
# missing; needs the guest's network). Nothing is left on the guest but
# /root/mailcheck and the packages.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VMDIR="${VMDIR:-$HOME/atrium-vm}"
VSSH="$ROOT/scripts/vssh"
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SCP="scp -i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -P 2222"
W=/root/mailcheck
say() { printf '\n=== %s\n' "$*"; }
die() { printf '\nFAILED: %s\n' "$*" >&2; exit 1; }

say "cross-build (mail-parse-jail, mail-sync-worker, the dovecot tests)"
set -- env; . "$ROOT/scripts/cloud-vm.sh"; set --
S="$VMDIR/sysroot"
export CC_aarch64_unknown_freebsd=clang AR_aarch64_unknown_freebsd=llvm-ar
export CFLAGS_aarch64_unknown_freebsd="--target=aarch64-unknown-freebsd --sysroot=$S"
export CARGO_INCREMENTAL=0
xb() { (cd "$ROOT/mail/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --target "$TARGET" --bin "$2") || die "build of $2"; }
xb mail-parse mail-parse-jail
xb mail-sync mail-sync-worker
(cd "$ROOT/mail/mail-engine" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo test -q --no-run --target "$TARGET" --test dovecot) || die "build of the dovecot tests"
T=$(ls -t "$ROOT/mail/mail-engine/target/$TARGET/debug/deps/"dovecot-* | grep -v '\.d$' | head -1)
[ -x "$T" ] || die "no test binary"

say "guest: Dovecot and Pigeonhole"
"$VSSH" "pkg info -e dovecot && pkg info -e dovecot-pigeonhole" || "$VSSH" "env ASSUME_ALWAYS_YES=yes pkg install -y dovecot dovecot-pigeonhole" || die "pkg install"
"$VSSH" "pw usershow dovenull >/dev/null 2>&1 || pw useradd dovenull -d /nonexistent -s /usr/sbin/nologin; rm -rf $W; mkdir -p $W" || die "guest setup"
for f in "$T" "$ROOT/mail/mail-parse/target/$TARGET/debug/mail-parse-jail" "$ROOT/mail/mail-sync/target/$TARGET/debug/mail-sync-worker"; do
    $SCP -q "$f" "root@localhost:$W/" || die "copy $(basename "$f")"
    [ "$(shasum -a 256 "$f" | cut -d' ' -f1)" = "$("$VSSH" "sha256 -q $W/$(basename "$f")")" ] || die "hash mismatch for $(basename "$f")"
done

say "guest: the tests"
"$VSSH" "cd $W && MAIL_PARSE_JAIL=$W/mail-parse-jail MAIL_SYNC_WORKER=$W/mail-sync-worker ./$(basename "$T") --test-threads 1 --nocapture 2>&1" | tee "$VMDIR/mail-vm-check.log"
grep -q '^test result: ok' "$VMDIR/mail-vm-check.log" || die "tests (log: $VMDIR/mail-vm-check.log)"
grep -q 'SKIPPED' "$VMDIR/mail-vm-check.log" && die "a test was skipped (Dovecot or Pigeonhole missing?)"
say "PASS"

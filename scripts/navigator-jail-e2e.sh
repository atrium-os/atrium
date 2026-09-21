#!/bin/sh
# End-to-end: the Navigator's corpus through real FreeBSD jails, one per document.
#
# ★★ COMMITTED BECAUSE THE AD-HOC VERSION IS WHERE THE RIGOUR LEAKS OUT.
# Every number in atrium-navigator-backend.md §4.7a and portcullis.md §6.5.2
# was produced by hand-typed ssh commands. That is unreproducible by anyone
# else, and — the part that actually bites — unreproducible by ME after a
# refactor. This script is those commands with the checks that kept saving
# them, so the evidence survives the session that made it.
#
# ★ THE CHECKS THAT ARE NOT OPTIONAL, each earned during that session:
#
#   - Staged binaries are gated on sha256 AT THE DESTINATION. Two staging
#     failures went undetected on the way here: the VM's /tmp is a 20M tmpfs
#     that a stale binary had filled, and zsh does not word-split an unquoted
#     command variable, so a copy loop silently ran a command named
#     "scp -i …". Either would have had a whole session measuring a binary
#     from a different day.
#   - Cheapest stages first, die on the first failure. A cross-build error
#     should not cost a corpus run to discover.
#   - Cleanup on EXIT, including the daemon this script starts.
#   - Leak checks are assertions, not prints: jails, mounts and upper dirs
#     must all be gone afterwards, and a pool that leaks one per run is a
#     pool that leaks invisibly.
#
# usage: scripts/navigator-jail-e2e.sh [--quick] [--recordings <dir>]
#          --quick        one document instead of the whole corpus
#          --recordings   host directory of emitted recordings
#                         (default: whatever PRERENDER_EMIT_DIR last wrote,
#                          passed explicitly because there is no good default)
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VSSH="$ROOT/scripts/vssh"
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SCP_OPTS="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o ConnectTimeout=30 -P 2222"
TARGET="aarch64-unknown-freebsd"
APP_ID="org.atrium.navigator.worker"
SOCK="/tmp/pd-e2e.sock"
QUICK=0
RECDIR=""

while [ $# -gt 0 ]; do
    case "$1" in
        --quick) QUICK=1 ;;
        --recordings) shift; RECDIR="${1:-}" ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

say()  { printf '\n=== %s\n' "$*"; }
die()  { printf '\nFAILED [%s]: %s\n' "$STAGE" "$*" >&2; exit 1; }
STAGE="startup"

# ★ Cleanup runs on EVERY exit, including a die() in the middle. A script that
# leaves a daemon and a pile of mounts behind makes the NEXT run's result a
# lie about a dirty machine.
cleanup() {
    [ -n "${NOCLEAN:-}" ] && return
    "$VSSH" "pkill -f 'portcullisd --socket $SOCK' >/dev/null 2>&1
             for j in \$(jls name | grep navigator_worker); do jail -r \"\$j\"; done
             for m in \$(mount -p | awk '{print \$2}' | grep navigator_worker | sort -r); do
                 umount -f \"\$m\" >/dev/null 2>&1
             done
             rm -f $SOCK" >/dev/null 2>&1
}
trap cleanup EXIT INT TERM

# ---- stage 1: the host suites, because they are the cheapest ---------------
STAGE="host tests"
say "$STAGE"
for c in navigator-dom navigator-backend; do
    (cd "$ROOT/$c" && cargo test --quiet >/dev/null 2>&1) || die "$c tests failed"
    echo "  $c ok"
done
(cd "$ROOT/portcullis" && cargo test --workspace --quiet >/dev/null 2>&1) \
    || die "portcullis workspace tests failed"
echo "  portcullis ok"

# ---- stage 2: cross-build --------------------------------------------------
STAGE="cross-build"
say "$STAGE  (host only — the VM is for RUNNING)"
(cd "$ROOT/portcullis" && cargo +nightly build --quiet --target "$TARGET" \
    -Z build-std=std,panic_abort -p portcullis-cli -p portcullisd -p opifex) \
    || die "portcullis cross-build"
(cd "$ROOT/navigator-backend" && cargo +nightly build --quiet --target "$TARGET" \
    -Z build-std=std,panic_abort --bin navigator-worker --example jailed_corpus) \
    || die "navigator cross-build"
echo "  built"

# ---- stage 3: is the VM even there? ---------------------------------------
STAGE="vm reachable"
say "$STAGE"
"$VSSH" 'true' >/dev/null 2>&1 || die "cannot reach the VM (is run-vm.sh up?)"
FREE=$("$VSSH" "df -k /root | tail -1 | awk '{print \$4}'")
[ "${FREE:-0}" -gt 400000 ] || die "only ${FREE}k free on /root; staging needs ~400M"
echo "  up, ${FREE}k free"

# ---- stage 4: stage, gated on the hash AT THE DESTINATION ------------------
STAGE="stage binaries"
say "$STAGE  (sha256 verified in the guest)"
stage() {
    src="$1"; name="$2"
    [ -f "$src" ] || die "missing build output: $src"
    h=$(shasum -a 256 "$src" | cut -d' ' -f1)
    # Remove first: a running binary is ETXTBSY and scp fails half-way.
    "$VSSH" "rm -f /root/$name" >/dev/null 2>&1
    # shellcheck disable=SC2086
    scp $SCP_OPTS "$src" "root@localhost:/root/$name" >/dev/null 2>&1 \
        || die "scp $name"
    g=$("$VSSH" "sha256 -q /root/$name" 2>/dev/null)
    [ "$h" = "$g" ] || die "$name hash mismatch: host=$h guest=$g (a stale binary would have been measured)"
    "$VSSH" "chmod +x /root/$name" >/dev/null 2>&1
    echo "  $name ok"
}
stage "$ROOT/portcullis/target/$TARGET/debug/portcullis"        portcullis
stage "$ROOT/portcullis/target/$TARGET/debug/portcullisd"       portcullisd
stage "$ROOT/portcullis/target/$TARGET/debug/opifex"            opifex
stage "$ROOT/navigator-backend/target/$TARGET/debug/navigator-worker" navigator-worker
stage "$ROOT/navigator-backend/target/$TARGET/debug/examples/jailed_corpus" jailed_corpus

# ---- stage 5: a SIGNED worker bundle, installed with its lib closure -------
STAGE="install worker"
say "$STAGE  (signed; opifex resolves the lib closure)"
"$VSSH" "set -e
    test -f /root/e2e-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/e2e-key.pem
    openssl ec -in /root/e2e-key.pem -pubout -out /root/e2e-pub.pem 2>/dev/null
    mkdir -p /etc/atrium/publishers && cp /root/e2e-pub.pem /etc/atrium/publishers/e2e.pem
    rm -rf /root/e2e-bundle && mkdir -p /root/e2e-bundle/bin
    cp /root/navigator-worker /root/e2e-bundle/bin/navigator-worker
    printf '[app]\nid = \"$APP_ID\"\nname = \"Navigator document worker\"\nversion = \"1\"\nentry = \"bin/navigator-worker\"\n' > /root/e2e-bundle/atrium.toml
    openssl dgst -sha256 -sign /root/e2e-key.pem -out /root/e2e-bundle/atrium.toml.sig /root/e2e-bundle/atrium.toml
    /root/opifex install /root/e2e-bundle" >/dev/null 2>&1 \
    || die "could not build/sign/install the worker bundle"
"$VSSH" "test -x /var/lib/atrium/apps/$APP_ID/bin/navigator-worker" \
    || die "the installed bundle has no entry binary"
echo "  installed with lib closure"

# ---- stage 6: recordings ---------------------------------------------------
STAGE="stage recordings"
say "$STAGE"
[ -n "$RECDIR" ] || die "pass --recordings <dir> (emit them with PRERENDER_EMIT_DIR)"
[ -d "$RECDIR" ] || die "no such recordings directory: $RECDIR"
HOSTN=$(ls "$RECDIR"/*.json 2>/dev/null | wc -l | tr -d ' ')
[ "$HOSTN" -gt 0 ] || die "no .json recordings in $RECDIR"
if [ "$QUICK" = 1 ]; then
    ONE=$(ls "$RECDIR"/*.json | head -1); HOSTN=1
    "$VSSH" "rm -rf /root/e2e-rec && mkdir -p /root/e2e-rec" >/dev/null 2>&1
    # shellcheck disable=SC2086
    scp $SCP_OPTS "$ONE" "root@localhost:/root/e2e-rec/" >/dev/null 2>&1 || die "scp recording"
else
    tar czf /tmp/e2e-rec.tgz -C "$RECDIR" . 2>/dev/null
    "$VSSH" "rm -rf /root/e2e-rec && mkdir -p /root/e2e-rec" >/dev/null 2>&1
    # shellcheck disable=SC2086
    scp $SCP_OPTS /tmp/e2e-rec.tgz "root@localhost:/root/e2e-rec.tgz" >/dev/null 2>&1 \
        || die "scp recordings"
    # ★ macOS tar writes ._ AppleDouble sidecars; they are not recordings and
    # the validator correctly refuses them. Removed here so a staging artefact
    # cannot be reported as a conversion failure.
    "$VSSH" "cd /root/e2e-rec && tar xzf /root/e2e-rec.tgz >/dev/null 2>&1; rm -f ._* .*/._*" >/dev/null 2>&1
fi
GUESTN=$("$VSSH" "ls /root/e2e-rec/*.json 2>/dev/null | wc -l" | tr -d ' ')
[ "$HOSTN" = "$GUESTN" ] || die "staged $GUESTN recordings, expected $HOSTN"
echo "  $GUESTN recordings"

# ---- stage 7: the daemon ---------------------------------------------------
STAGE="daemon"
say "$STAGE"
"$VSSH" "pkill -f 'portcullisd --socket $SOCK' >/dev/null 2>&1
         rm -f $SOCK
         /root/portcullisd --socket $SOCK > /root/e2e-pd.log 2>&1 &
         sleep 2
         pgrep -f 'portcullisd --socket $SOCK' > /dev/null" \
    || die "portcullisd did not start: $("$VSSH" "cat /root/e2e-pd.log" 2>/dev/null)"
echo "  up on $SOCK"

# ---- stage 8: the corpus, through daemon-created jails ---------------------
STAGE="corpus"
say "$STAGE  (one jail per document, created by portcullisd)"
OUT=$("$VSSH" "export PORTCULLIS_SOCKET=$SOCK
    /root/jailed_corpus /root/e2e-rec $APP_ID /root/portcullis exec --daemon --instance '{instance}' 2>/dev/null")
echo "$OUT" | sed 's/^/  /'
echo "$OUT" | grep -q '^OK$' || die "corpus run did not report OK"
# ★ Gate on the NUMBERS, not on the word OK: a run that opened nothing also
# has no failures.
SESS=$(echo "$OUT" | sed -n 's/^\([0-9]*\) sessions.*/\1/p')
[ "${SESS:-0}" = "$GUESTN" ] || die "opened $SESS sessions for $GUESTN recordings"

# ---- stage 9: leaks are assertions ----------------------------------------
STAGE="leaks"
say "$STAGE"
LJ=$("$VSSH" "jls name | grep -c navigator_worker" | tr -d ' ')
LM=$("$VSSH" "mount -p | grep -c navigator_worker" | tr -d ' ')
LU=$("$VSSH" "ls /var/run/portcullis-exec 2>/dev/null | wc -l" | tr -d ' ')
[ "${LJ:-0}" = 0 ] || die "$LJ jails leaked"
[ "${LM:-0}" = 0 ] || die "$LM mounts leaked"
[ "${LU:-0}" = 0 ] || die "$LU writable layers leaked in /var/run/portcullis-exec"
echo "  no jails, no mounts, no upper dirs"

# ---- stage 10: the refusals must still refuse ------------------------------
STAGE="refusals"
say "$STAGE"
# Unsigned: install the same bundle without a signature and expect a refusal.
"$VSSH" "rm -rf /root/e2e-unsigned && cp -r /root/e2e-bundle /root/e2e-unsigned
         rm -f /root/e2e-unsigned/atrium.toml.sig
         sed -i '' -e 's/^id = .*/id = \"test.e2e.unsigned\"/' /root/e2e-unsigned/atrium.toml 2>/dev/null ||
         sed -i -e 's/^id = .*/id = \"test.e2e.unsigned\"/' /root/e2e-unsigned/atrium.toml" >/dev/null 2>&1
UNS=$("$VSSH" "export PORTCULLIS_SOCKET=$SOCK
    /root/portcullis exec --daemon --instance u /root/e2e-unsigned < /dev/null 2>&1 | tail -1")
case "$UNS" in
    *REFUSED*|*not\ signed*|*trust*) echo "  unsigned refused" ;;
    *) die "an UNSIGNED bundle was not refused: $UNS" ;;
esac
# A duplicate live instance tag must be refused without harming the first.
# ★ The first worker has to still be ALIVE for this to test anything. An
# earlier version piped `echo x`, which is a malformed frame: the worker
# exited immediately, the jail was gone before the second attempt, and the
# check reported nothing at all rather than failing. A valid OPEN frame plus
# a held-open pipe keeps it blocked on its next read.
DUP=$("$VSSH" "export PORTCULLIS_SOCKET=$SOCK
    F=\$(ls /root/e2e-rec/*.json | head -1); LEN=\$(wc -c < \"\$F\" | tr -d ' ')
    ( printf 'OPEN %s\\n' \"\$LEN\"; cat \"\$F\"; sleep 6 ) |
        /root/portcullis exec --daemon --instance dup $APP_ID >/dev/null 2>&1 &
    sleep 3
    /root/portcullis exec --daemon --instance dup $APP_ID < /dev/null 2>&1 | tail -1
    wait")
case "$DUP" in
    *already\ running*) echo "  duplicate instance refused" ;;
    *) die "a duplicate live instance tag was not refused: $DUP" ;;
esac

say "PASS  ($GUESTN documents, $GUESTN jails, no leaks, refusals intact)"

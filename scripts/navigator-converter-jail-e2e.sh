#!/bin/sh
# End-to-end: the CONVERTER — the process that runs page JavaScript — in a
# one-shot, zero-capability jail per conversion (threat model §2.2, P0), and
# the fetcher's §2.1 address rules under capability mode inside its own jail.
#
# Sibling of navigator-jail-e2e.sh (the document worker) and built the same
# way, with the same non-optional checks: binaries gated on sha256 AT THE
# DESTINATION, cheapest stages first, cleanup on EXIT, leaks as assertions
# (live AND dying jails, jaild zombies, mounts, upper dirs), and every refusal
# check distinguishing "the product did not refuse" from "I could not ask".
#
# What it proves:
#   1. navigator-fetchd --gate passes in the VM, unjailed and jailed, with the
#      §2.1 rows: localhost (resolved BY CASPER), 127.0.0.1, 169.254.169.254,
#      [::1], [::ffff:127.0.0.1], 10.0.0.1, 2130706433 refused; the grant's
#      control arm admits localhost; metadata refused even under the grant.
#   2. The corpus converts through jailed converters (one jail per document,
#      created by portcullisd for an unprivileged broker) and the recordings,
#      blobs and reports are BYTE-IDENTICAL to unconfined serve-mode runs.
#      Offline (PRERENDER_NET unset), so the comparison has no network in it.
#      Arms interleave A B A B … so drift lands on both.
#   3. A networked subset through a jailed converter AND a jailed fetchd.
#   4. The require-confinement switch: an unconfined run with
#      PRERENDER_REQUIRE_CONFINEMENT=1 is refused, and a jailed one is not.
#   5. No jails, mounts, upper dirs or zombies are left behind.
#
# usage: scripts/navigator-converter-jail-e2e.sh [--rounds N] [--net N] [--html <dir>]
#          --rounds N   interleaved rounds of A (unconfined) and B (jailed); default 2
#          --net N      networked documents for stage 3; default 5, 0 skips it
#          --html dir   pages + manifest.tsv; default ~/src/navigator-corpus/batch2/html
#
# Environment: TARGET (default aarch64-unknown-freebsd), CROSS_RUSTFLAGS and
# SYSROOT for a non-Mac host (`. scripts/cloud-vm.sh env` sets the first two;
# SYSROOT defaults to ~/atrium-vm/sysroot there). BROKER_USER (default navtest).
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VSSH="$ROOT/scripts/vssh"
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SCP_OPTS="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o ConnectTimeout=30 -P 2222"
TARGET="${TARGET:-aarch64-unknown-freebsd}"
CONV_ID="org.atrium.navigator.converter"
FETCH_ID="org.atrium.navigator.fetcher"
SOCK="/tmp/pd-conv-e2e.sock"
BROKER_USER="${BROKER_USER:-navtest}"
BIN="/root/navigator-conv-e2e"
ROUNDS=2
NETN=5
HTML="$HOME/src/navigator-corpus/batch2/html"
case "$(uname -s)" in
    Darwin) SYSROOT="${SYSROOT:-$ROOT/sysroot}" ;;
    *)      SYSROOT="${SYSROOT:-$HOME/atrium-vm/sysroot}" ;;
esac

while [ $# -gt 0 ]; do
    case "$1" in
        --rounds) shift; ROUNDS="${1:-2}" ;;
        --net) shift; NETN="${1:-5}" ;;
        --html) shift; HTML="${1:-}" ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

say()  { printf '\n=== %s\n' "$*"; }
die()  { printf '\nFAILED [%s]: %s\n' "$STAGE" "$*" >&2; exit 1; }
STAGE="startup"

cleanup() {
    [ -n "${NOCLEAN:-}" ] && return
    "$VSSH" "pkill -f 'portcullisd --socket $SOCK' >/dev/null 2>&1
             for j in \$(jls name | grep -E 'navigator[-_](converter|fetcher)'); do jail -r \"\$j\"; done
             for m in \$(mount -p | awk '{print \$2}' | grep -E 'navigator[-_](converter|fetcher)' | sort -r); do
                 umount -f \"\$m\" >/dev/null 2>&1
             done
             rm -f $SOCK" >/dev/null 2>&1
}
trap cleanup EXIT INT TERM

# ---- stage 1: host suites ----------------------------------------------------
STAGE="host tests"
say "$STAGE"
for c in navigator-fetch navigator-prerender navigator-backend; do
    (cd "$ROOT/$c" && cargo test --quiet >/dev/null 2>&1) || die "$c tests failed"
    echo "  $c ok"
done

# ---- stage 2: cross-build (host only — the VM is for RUNNING) ----------------
STAGE="cross-build"
say "$STAGE"
[ -n "${CROSS_RUSTFLAGS:-}" ] && export RUSTFLAGS="$CROSS_RUSTFLAGS"
T=$(echo "$TARGET" | tr - _)
# ring (navigator-fetch's TLS) compiles C: it needs a FreeBSD-targeting clang
# and the sysroot's headers. Honour an explicit CC_<target>; otherwise derive it.
if ! env | grep -q "^CC_$T="; then
    CLANG=$(command -v /opt/homebrew/opt/llvm/bin/clang || command -v clang) || die "no clang for ring"
    [ -d "$SYSROOT/usr/include" ] || die "no sysroot at $SYSROOT (set SYSROOT)"
    export "CC_$T=$CLANG" "CFLAGS_$T=--target=$TARGET --sysroot=$SYSROOT"
    AR=$(command -v /opt/homebrew/opt/llvm/bin/llvm-ar || command -v llvm-ar || command -v ar)
    export "AR_$T=$AR"
fi
B="cargo +nightly build --quiet --release --target $TARGET -Z build-std=std,panic_abort"
(cd "$ROOT/portcullis" && cargo +nightly build --quiet --target "$TARGET" -Z build-std=std,panic_abort \
    -p portcullis-cli -p portcullisd -p opifex) || die "portcullis cross-build"
# ★ RELEASE for the converter: Boa under a debug build is slow enough that the
# deadline, not the jail, would decide the comparison.
(cd "$ROOT/navigator-prerender" && $B --bin prerender) || die "prerender cross-build"
(cd "$ROOT/navigator-fetch" && $B --bin navigator-fetchd) || die "navigator-fetchd cross-build"
unset RUSTFLAGS
echo "  built"

# ---- stage 3: VM ---------------------------------------------------------------
STAGE="vm reachable"
say "$STAGE"
"$VSSH" 'true' >/dev/null 2>&1 || die "cannot reach the VM"
"$VSSH" 'sysctl -n kern.ident kern.sched.name 2>/dev/null; uname -r' | sed 's/^/  /'
FREE=$("$VSSH" "df -k /root | tail -1 | awk '{print \$4}'")
[ "${FREE:-0}" -gt 600000 ] || die "only ${FREE}k free on /root; staging needs ~600M"

# ---- stage 4: stage binaries, gated on the hash in the guest -------------------
STAGE="stage binaries"
say "$STAGE"
stage() {
    src="$1"; name="$2"
    [ -f "$src" ] || die "missing build output: $src"
    h=$(shasum -a 256 "$src" 2>/dev/null | cut -d' ' -f1); [ -n "$h" ] || h=$(sha256sum "$src" | cut -d' ' -f1)
    # ★ Up to three copies. In the cloud VM (QEMU TCG) a large scp now and
    # then lands with a ~30 KB run of ZEROS in the middle, at full size: the
    # hash below catches it every time, and a fresh copy is clean. Retrying
    # never weakens the gate; a binary is used only once its hash matches.
    try=1
    while :; do
        "$VSSH" "mkdir -p $BIN && rm -f $BIN/$name" >/dev/null 2>&1
        # shellcheck disable=SC2086
        scp $SCP_OPTS "$src" "root@localhost:$BIN/$name" >/dev/null 2>&1 || die "scp $name"
        g=$("$VSSH" "sha256 -q $BIN/$name" 2>/dev/null)
        [ "$h" = "$g" ] && break
        [ $try -ge 3 ] && die "$name hash mismatch: host=$h guest=$g"
        echo "  $name hash mismatch on copy $try (guest=$g); copying again"
        try=$((try + 1))
    done
    "$VSSH" "chmod +x $BIN/$name" >/dev/null 2>&1
    echo "  $name ok"
}
P="$ROOT/portcullis/target/$TARGET/debug"
stage "$P/portcullis" portcullis
stage "$P/portcullisd" portcullisd
stage "$P/opifex" opifex
stage "$ROOT/navigator-prerender/target/$TARGET/release/prerender" prerender
stage "$ROOT/navigator-fetch/target/$TARGET/release/navigator-fetchd" navigator-fetchd

# ---- stage 5: the fetcher's gate, unjailed (threat model §2.1) -----------------
STAGE="fetchd gate (unjailed)"
say "$STAGE"
G=$("$VSSH" "$BIN/navigator-fetchd --gate 2>&1") || true
echo "$G" | sed 's/^/  /'
echo "$G" | grep -q '^M1 GATE: PASS$' || die "the gate did not pass"
for row in 'localhost' '169.254.169.254' 'ffff:127.0.0.1' 'grant admits localhost' 'metadata refused even under the grant'; do
    echo "$G" | grep "^PASS" | grep -q -- "$row" || die "gate row missing or failing: $row"
done

# ---- stage 6: signed bundles, installed with their lib closures ----------------
STAGE="install bundles"
say "$STAGE"
# ★ The converter declares NO capabilities: no network, no mounts. Its only
# channels are the three descriptors portcullis exec hands it.
"$VSSH" "set -e
    test -f /root/e2e-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/e2e-key.pem
    openssl ec -in /root/e2e-key.pem -pubout -out /root/e2e-pub.pem 2>/dev/null
    mkdir -p /etc/atrium/publishers && cp /root/e2e-pub.pem /etc/atrium/publishers/e2e.pem
    rm -rf /root/conv-bundle && mkdir -p /root/conv-bundle/bin
    cp $BIN/prerender /root/conv-bundle/bin/prerender
    printf '[app]\nid = \"$CONV_ID\"\nname = \"Navigator converter\"\nversion = \"1\"\nentry = \"bin/prerender\"\n\n[resources]\nmemory = \"4G\"\ncpu = 100\n\n[resources.pool]\nweight = 1.0\nfloor = \"256M\"\n' > /root/conv-bundle/atrium.toml
    openssl dgst -sha256 -sign /root/e2e-key.pem -out /root/conv-bundle/atrium.toml.sig /root/conv-bundle/atrium.toml
    $BIN/opifex install /root/conv-bundle
    rm -rf /root/fetch-bundle && mkdir -p /root/fetch-bundle/bin /root/fetch-bundle/etc/ssl
    cp $BIN/navigator-fetchd /root/fetch-bundle/bin/navigator-fetchd
    cp /etc/ssl/cert.pem /root/fetch-bundle/etc/ssl/cert.pem
    cp /etc/resolv.conf /etc/hosts /etc/nsswitch.conf /root/fetch-bundle/etc/ 2>/dev/null || true
    printf '[app]\nid = \"$FETCH_ID\"\nname = \"Navigator fetcher\"\nversion = \"1\"\nentry = \"bin/navigator-fetchd\"\n\n[capabilities]\nnetwork = \"full\"\n' > /root/fetch-bundle/atrium.toml
    openssl dgst -sha256 -sign /root/e2e-key.pem -out /root/fetch-bundle/atrium.toml.sig /root/fetch-bundle/atrium.toml
    $BIN/opifex install /root/fetch-bundle
    $BIN/portcullis policy grant --system $FETCH_ID" >/dev/null 2>&1 \
    || die "could not build/sign/install the bundles"
"$VSSH" "test -x /var/lib/atrium/apps/$CONV_ID/bin/prerender && test -x /var/lib/atrium/apps/$FETCH_ID/bin/navigator-fetchd" \
    || die "an installed bundle has no entry binary"
echo "  $CONV_ID (no capabilities), $FETCH_ID (network = full, system grant)"

# ---- stage 7: the corpus, staged into the broker's own home ---------------------
STAGE="stage corpus"
say "$STAGE"
[ -d "$HTML" ] && [ -f "$HTML/manifest.tsv" ] || die "no pages + manifest.tsv in $HTML"
HOSTN=$(ls "$HTML"/*.html | wc -l | tr -d ' ')
TGZ="${TMPDIR:-/tmp}/conv-e2e-html.$$.tgz"
COPYFILE_DISABLE=1 tar czf "$TGZ" -C "$HTML" . 2>/dev/null
"$VSSH" "id $BROKER_USER" >/dev/null 2>&1 || die "no broker user $BROKER_USER (pw useradd $BROKER_USER -m)"
BH=$("$VSSH" "getent passwd $BROKER_USER | cut -d: -f6" | tr -d '\r')
BW="$BH/conv-e2e"
# shellcheck disable=SC2086
scp $SCP_OPTS "$TGZ" "root@localhost:/root/conv-e2e-html.tgz" >/dev/null 2>&1 || die "scp corpus"
rm -f "$TGZ"
"$VSSH" "rm -rf $BW && mkdir -p $BW/html && tar xzf /root/conv-e2e-html.tgz -C $BW/html && rm -f $BW/html/._* &&
         cp $BIN/prerender $BIN/portcullis $BW/ && chown -R $BROKER_USER $BW" >/dev/null 2>&1 || die "stage corpus"
GUESTN=$("$VSSH" "ls $BW/html/*.html | wc -l" | tr -d ' ')
[ "$HOSTN" = "$GUESTN" ] || die "staged $GUESTN pages, expected $HOSTN"
echo "  $GUESTN pages in $BW (broker $BROKER_USER)"
AS_BROKER="su -m $BROKER_USER -c"

# ---- stage 8: the daemon ----------------------------------------------------------
STAGE="daemon"
say "$STAGE"
"$VSSH" "pkill -f 'portcullisd --socket $SOCK' >/dev/null 2>&1; rm -f $SOCK
         $BIN/portcullisd --socket $SOCK > /root/conv-e2e-pd.log 2>&1 &
         sleep 2; pgrep -f 'portcullisd --socket $SOCK' > /dev/null" \
    || die "portcullisd did not start: $("$VSSH" "cat /root/conv-e2e-pd.log" 2>/dev/null)"
echo "  up on $SOCK"
LAUNCH="$BW/portcullis exec --daemon --instance conv{instance} $CONV_ID"
FETCHD="$BW/portcullis exec --daemon --instance f{instance} $FETCH_ID"

# ---- stage 9: the fetcher's gate INSIDE its jail ------------------------------------
STAGE="fetchd gate (jailed)"
say "$STAGE"
GJ=$("$VSSH" "export PORTCULLIS_SOCKET=$SOCK; echo --gate | $AS_BROKER \"$BW/portcullis exec --daemon --instance g1 $FETCH_ID\" 2>&1") \
    || true
echo "$GJ" | sed 's/^/  /'
echo "$GJ" | grep -q '^M1 GATE: PASS$' || die "the gate did not pass inside the jail"

# ---- stage 9b: resource limits through jaild (portcullis.md §6.5.2e) -------------------
STAGE="limits"
say "$STAGE"
RACCT=$("$VSSH" "sysctl -n kern.racct.enable 2>/dev/null" | tr -d '\r')
ALLOW=$("$VSSH" "awk '/^\[resource_control\]/{s=1;next} /^\[/{s=0} s && /allow_rctl/ {print \$3}' /etc/atrium/jaild.policy.toml 2>/dev/null" | tr -d '\r')
echo "  kern.racct.enable=${RACCT:-?}  jaild allow_rctl=${ALLOW:-false}"
if [ "$RACCT" = 1 ] && [ "$ALLOW" != true ]; then
    die "RACCT is on but jaild's policy refuses rctl: set allow_rctl = true under [resource_control] in /etc/atrium/jaild.policy.toml and restart jaild (the manifests declare limits, and jaild refuses rather than create them uncapped)"
fi
if [ "$RACCT" = 1 ]; then
    # A page that spins for its whole deadline: long enough to read the rules
    # jaild set on its jail while it runs.
    "$VSSH" "mkdir -p $BW/spin && printf '<script>for(;;){}</script>' > $BW/spin/spin.html && chown -R $BROKER_USER $BW/spin" >/dev/null 2>&1
    "$VSSH" "export PORTCULLIS_SOCKET=$SOCK; cd $BW && $AS_BROKER 'env PRERENDER_REQUIRE_CONFINEMENT=1 PRERENDER_LAUNCHER=\"$LAUNCH\" PRERENDER_DEADLINE=8 ./prerender spin > spin/report.txt 2>&1' &
             sleep 4; rctl | grep -E 'navigator-converter' > /root/conv-e2e-rctl.txt; wait" >/dev/null 2>&1
    RULES=$("$VSSH" "cat /root/conv-e2e-rctl.txt")
    echo "$RULES" | sed 's/^/  during: /'
    echo "$RULES" | grep -qE 'memoryuse:sigkill=(4096M|4294967296|4\.0G)' || die "no memoryuse rule on the converter jail while it ran"
    echo "$RULES" | grep -q 'pcpu:deny=100' || die "no pcpu rule on the converter jail while it ran"
    "$VSSH" "grep -q 'timed out' $BW/spin/report.txt" || die "the spinning page was not stopped: $("$VSSH" "tail -3 $BW/spin/report.txt")"
    sleep 2
    LEFT=$("$VSSH" "rctl | grep -cE 'navigator-(converter|fetcher|worker)'" | tr -d ' ')
    [ "${LEFT:-0}" = 0 ] || die "$LEFT rctl rules outlived their jails: $("$VSSH" "rctl | grep -E 'navigator-'")"
    echo "  rules present while the jail ran, gone after it"
else
    echo "  SKIPPED the rule checks: kern.racct.enable=0 (a loader tunable; the converter ran UNCAPPED"
    echo "  and portcullis said so on stderr). Boot with kern.racct.enable=1 to verify §6.5.2e."
fi

# ---- stage 10: the refusal must refuse ------------------------------------------------
STAGE="require-confinement"
say "$STAGE"
R=$("$VSSH" "cd $BW && $AS_BROKER 'PRERENDER_REQUIRE_CONFINEMENT=1 ./prerender html' 2>&1 | tail -1") \
    || die "could not reach the VM (transport, not product)"
[ -n "$R" ] || die "no output from the refusal check — unreached, not a pass"
case "$R" in *REFUSED*PRERENDER_LAUNCHER*) echo "  unconfined run refused" ;; *) die "an unconfined run was NOT refused: $R" ;; esac

# ---- stage 11: A (unconfined) vs B (jailed), interleaved, offline ----------------------
STAGE="corpus A/B"
say "$STAGE  ($ROUNDS rounds, $GUESTN pages, offline, exploration on)"
# ★ The output directory is made by root (ssh) but written by the broker:
# without the chown every redirect failed, and the arms "produced no
# recordings" because the report file could not even be created.
run_arm() { # $1 = arm name, $2 = round, $3 = extra env
    "$VSSH" "export PORTCULLIS_SOCKET=$SOCK; cd $BW && rm -rf out-$1-$2 && mkdir out-$1-$2 && chown $BROKER_USER out-$1-$2 &&
        $AS_BROKER 'env $3 PRERENDER_EXPLORE=1 PRERENDER_DEADLINE=120 PRERENDER_EMIT_DIR=$BW/out-$1-$2/rec \
                    PRERENDER_BLOB_DIR=$BW/out-$1-$2/blobs ./prerender html > out-$1-$2/report.txt 2> out-$1-$2/err.txt'
        jls -d name | grep -cE 'navigator[-_]converter' || true" >/dev/null 2>&1
}
r=1
while [ $r -le "$ROUNDS" ]; do
    run_arm A $r ""
    run_arm B $r "PRERENDER_REQUIRE_CONFINEMENT=1 PRERENDER_LAUNCHER=\"$LAUNCH\""
    r=$((r + 1))
done
"$VSSH" "cd $BW && for d in out-*; do printf '%s ' \$d; head -1 \$d/report.txt; grep 'conversion wall' \$d/report.txt; \
         printf '    recordings %s  blobs %s  failed: ' \$(ls \$d/rec 2>/dev/null | wc -l) \$(ls \$d/blobs 2>/dev/null | wc -l); \
         grep -c '^      ' \$d/report.txt; done" | sed 's/^/  /'
# ★ The comparison: every jailed round against round-1 unconfined, recordings
# and blobs byte for byte, reports with the two lines that MUST differ (the
# confinement banner and the wall-time line) removed.
CMP=$("$VSSH" "cd $BW && fails=0; r=1; while [ \$r -le $ROUNDS ]; do
        for arm in A B; do
            diff -r out-A-1/rec out-\$arm-\$r/rec >/dev/null || { echo \"recordings differ: \$arm-\$r\"; fails=1; }
            # ★ A corpus with no images writes no blob dir at all; two absent
            # dirs are equal, one absent and one present are not.
            if [ -d out-A-1/blobs ] || [ -d out-\$arm-\$r/blobs ]; then
                diff -r out-A-1/blobs out-\$arm-\$r/blobs >/dev/null 2>&1 || { echo \"blobs differ: \$arm-\$r\"; fails=1; }
            fi
            grep -v -e '^confinement:' -e 'conversion wall' out-A-1/report.txt > /tmp/ra.\$\$
            grep -v -e '^confinement:' -e 'conversion wall' out-\$arm-\$r/report.txt > /tmp/rb.\$\$
            cmp -s /tmp/ra.\$\$ /tmp/rb.\$\$ || { echo \"reports differ: \$arm-\$r\"; fails=1; }
        done; r=\$((r+1)); done; rm -f /tmp/ra.\$\$ /tmp/rb.\$\$; echo \"fails=\$fails\"")
echo "$CMP" | sed 's/^/  /'
N=$("$VSSH" "ls $BW/out-B-1/rec | wc -l" | tr -d ' ')
[ "${N:-0}" -gt 0 ] || die "the jailed arm produced no recordings"
echo "$CMP" | grep -q '^fails=0$' || die "jailed and unconfined conversions differ"
JN=$("$VSSH" "grep -c 'confinement: LAUNCHER' $BW/out-B-1/report.txt" | tr -d ' ')
[ "$JN" = 1 ] || die "the B arm did not run with the launcher"

# ---- stage 12: networked, jailed converter + jailed fetcher -----------------------------
if [ "$NETN" -gt 0 ]; then
    STAGE="networked"
    say "$STAGE  ($NETN pages; converter AND fetcher jailed)"
    "$VSSH" "cd $BW && rm -rf neth && mkdir neth && for f in \$(ls html/*.html | head -$NETN); do cp \$f neth/; \
             grep -F \"\$(basename \$f)\" html/manifest.tsv >> neth/manifest.tsv; done; chown -R $BROKER_USER neth" >/dev/null 2>&1
    for arm in A B; do
        extra=""; [ $arm = B ] && extra="PRERENDER_REQUIRE_CONFINEMENT=1 PRERENDER_LAUNCHER=\"$LAUNCH\""
        "$VSSH" "export PORTCULLIS_SOCKET=$SOCK; cd $BW && rm -rf net-$arm && mkdir net-$arm && chown $BROKER_USER net-$arm &&
            $AS_BROKER 'env $extra PRERENDER_NET=1 PRERENDER_FETCHD=\"$FETCHD\" PRERENDER_EXPLORE=1 PRERENDER_DEADLINE=180 \
                PRERENDER_EMIT_DIR=$BW/net-$arm/rec PRERENDER_BLOB_DIR=$BW/net-$arm/blobs ./prerender neth > net-$arm/report.txt 2> net-$arm/err.txt'" \
            >/dev/null 2>&1
    done
    "$VSSH" "cd $BW && for a in A B; do printf '%s ' \$a; grep 'conversion wall' net-\$a/report.txt; \
             printf '    recordings %s blobs %s fetchd requests (G): %s\n' \$(ls net-\$a/rec | wc -l) \$(ls net-\$a/blobs 2>/dev/null | wc -l) \
             \"\$(grep -c 'fetch' net-\$a/report.txt)\"; done; \
             echo '  differing recordings (live network; reported, not gated):'; diff -rq net-A/rec net-B/rec | sed 's/^/    /'" | sed 's/^/  /'
    NB=$("$VSSH" "ls $BW/net-B/rec 2>/dev/null | wc -l" | tr -d ' ')
    [ "${NB:-0}" -gt 0 ] || die "no networked recordings through the jailed converter: $("$VSSH" "tail -5 $BW/net-B/report.txt")"
fi

# ---- stage 13: leaks are assertions ------------------------------------------------------
STAGE="leaks"
say "$STAGE"
PAT='navigator[-_](converter|fetcher)'
LJ=$("$VSSH" "jls name | grep -cE '$PAT'" | tr -d ' ')
LD=$("$VSSH" "jls -d name | grep -cE '$PAT'" | tr -d ' ')
LM=$("$VSSH" "mount -p | grep -cE '$PAT'" | tr -d ' ')
LU=$("$VSSH" "ls /var/run/portcullis-exec 2>/dev/null | wc -l" | tr -d ' ')
LZ=$("$VSSH" "J=\$(pgrep -f 'atrium-jaild serve'); [ -z \"\$J\" ] && echo 0 || ps -ax -o ppid -o stat | awk -v j=\$J '\$1==j && \$2 ~ /Z/' | wc -l" | tr -d ' ')
[ "${LJ:-0}" = 0 ] || die "$LJ jails leaked"
[ "${LD:-0}" = 0 ] || die "$LD jails left DYING"
[ "${LZ:-0}" = 0 ] || die "$LZ zombie children of jaild"
[ "${LM:-0}" = 0 ] || die "$LM mounts leaked"
[ "${LU:-0}" = 0 ] || die "$LU writable layers leaked"
LR=$("$VSSH" "rctl 2>/dev/null | grep -cE '$PAT'" | tr -d ' ')
[ "${LR:-0}" = 0 ] || die "$LR rctl rules leaked (they must go with their jails)"
echo "  no jails (live or dying), no zombies, no mounts, no upper dirs, no rctl rules"

say "PASS  ($GUESTN pages × $ROUNDS rounds, jailed = unconfined byte for byte, no leaks)"

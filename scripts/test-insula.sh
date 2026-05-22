#!/bin/sh
# Run `cargo test` across every Insula-related crate
# and print a summary table. Intended for the local dev
# loop + CI integration.
#
# Usage:
#   scripts/test-insula.sh             # all crates
#   scripts/test-insula.sh --quick     # skip the heavy
#                                      # integration tests
#                                      # (atrium-paint /
#                                      # insula-clock /
#                                      # daemons-restart)
#
# Exits 0 iff every crate passes; non-zero otherwise.

set -u

# Crate order: foundation libs first, then daemons,
# then samples, then the CLI which depends on
# everything else.
ALL_CRATES="
insula-manifest
insula-bundle
libatrium
insula-host-macos
insula-logd
vestibulum-macos
atrium-netd-macos
praeco-macos
tabellarius-macos
tabellarius-relay
insula-hello
atrium-fetch
atrium-mon
atrium-paint
insula-clock
insula-cli
"

QUICK_SKIPS="atrium-paint insula-clock"

MODE=full
for arg in "$@"; do
    case "$arg" in
        --quick) MODE=quick ;;
        -h|--help)
            sed -n '2,16p' "$0"
            exit 0
            ;;
        *)
            printf 'unknown flag: %s\n' "$arg" >&2
            exit 2
            ;;
    esac
done

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT" || exit 1

total_passed=0
total_failed=0
failed_crates=""

for crate in $ALL_CRATES; do
    skip_this=0
    if [ "$MODE" = quick ]; then
        for skip in $QUICK_SKIPS; do
            if [ "$crate" = "$skip" ]; then
                skip_this=1
                break
            fi
        done
    fi
    if [ "$skip_this" = 1 ]; then
        printf '%-22s SKIP (--quick)\n' "$crate"
        continue
    fi

    if [ ! -f "$crate/Cargo.toml" ]; then
        printf '%-22s missing Cargo.toml\n' "$crate"
        total_failed=$((total_failed + 1))
        failed_crates="$failed_crates $crate"
        continue
    fi

    output=$(cargo test --manifest-path "$crate/Cargo.toml" --quiet 2>&1)
    status=$?
    passed=$(printf '%s\n' "$output" | grep -oE '[0-9]+ passed' \
        | awk '{s+=$1} END {print s+0}')
    failed=$(printf '%s\n' "$output" | grep -oE '[0-9]+ failed' \
        | awk '{s+=$1} END {print s+0}')

    if [ "$status" = 0 ] && [ "$failed" = 0 ]; then
        printf '%-22s PASS  %4d test(s)\n' "$crate" "$passed"
        total_passed=$((total_passed + passed))
    else
        printf '%-22s FAIL  %4d passed, %d failed (status=%d)\n' \
            "$crate" "$passed" "$failed" "$status"
        total_passed=$((total_passed + passed))
        total_failed=$((total_failed + failed))
        if [ "$total_failed" -eq 0 ] && [ "$status" != 0 ]; then
            total_failed=$((total_failed + 1))
        fi
        failed_crates="$failed_crates $crate"
    fi
done

echo
printf 'total: %d passed, %d failed\n' "$total_passed" "$total_failed"
if [ "$total_failed" != 0 ]; then
    printf 'failing crates:%s\n' "$failed_crates" >&2
    exit 1
fi

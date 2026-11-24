# gate.sh: pass/fail bookkeeping for check scripts, sourced, POSIX sh.
#
#   . "$(dirname "$0")/lib/gate.sh"
#   gate_begin opifex-token 18     # name, and how many checks must run
#   check "the key is listed" 1 "$(... | grep -c key)"
#   ok "built";  bad "no answer"
#   gate_end                       # prints "NAME: N ok, M failed", returns 0 or 1
#
# The invariants, so a script cannot report green by accident:
#   - gate_end returns non-zero when any check failed, so `sh x.sh` fails;
#     end the script with it (or `gate_end; exit $?` before cleanup).
#   - A gate that ran fewer checks than it declared fails: a step that
#     stopped early, or a loop over nothing, is a failure, not a pass.
#   - A gate that ran no checks at all fails.
#   - gate_abort WHY stops at once with a failure (a missing tool, a build
#     that failed): it counts as a failed check and prints the summary.

GATE_NAME=${GATE_NAME:-$(basename "$0" .sh)}
GATE_EXPECT=0
GATE_OKS=0
GATE_FAILS=0

gate_begin() {
    GATE_NAME=$1
    GATE_EXPECT=${2:-0}
}

ok() {
    printf '  ok %s\n' "$*"
    GATE_OKS=$((GATE_OKS + 1))
}

bad() {
    printf '  FAIL %s\n' "$*"
    GATE_FAILS=$((GATE_FAILS + 1))
}

# check NAME EXPECTED GOT
check() {
    if [ "$2" = "$3" ]; then ok "$1"; else bad "$1: expected [$2] got [$3]"; fi
}

gate_end() {
    ran=$((GATE_OKS + GATE_FAILS))
    if [ "$ran" -eq 0 ]; then
        bad "no checks ran"
    elif [ "$ran" -lt "$GATE_EXPECT" ]; then
        bad "only $ran of $GATE_EXPECT checks ran"
    fi
    printf '%s: %d ok, %d failed\n' "$GATE_NAME" "$GATE_OKS" "$GATE_FAILS"
    [ "$GATE_FAILS" -eq 0 ]
}

gate_abort() {
    bad "$*"
    GATE_EXPECT=0  # the abort is the reason; do not also report the count
    gate_end
    exit 1
}

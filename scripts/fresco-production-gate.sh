#!/bin/sh
# Gate: a shipped frescod refuses the development modes that put clients'
# scenes back into frescod (officina.md §10.5; fresco-f4-split.md §F4i).
#
#   sh scripts/fresco-production-gate.sh            config + unit tests (any host)
#   sh scripts/fresco-production-gate.sh vm BIN     also start BIN (a frescod
#                                                   already in the VM) as
#                                                   atrium-frescod with each dev
#                                                   setting; each must exit 78
#
# Checks:
#  1. fresco_split::production's tests (the policy frescod runs first thing).
#  2. The shipped service (frescod/etc/services.d/40-atrium-frescod.toml) runs
#     /usr/local/bin/atrium-frescod as _frescod (65010), so the policy applies,
#     and sets none of the development variables.
#  3. Nothing that installs or launches the shipped frescod sets them either
#     (the rc.d script, the portcullisd service default).
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
pass=0
ok() { echo "ok   $1"; pass=$((pass + 1)); }
fail() { echo "FAIL $1"; exit 1; }

(cd "$ROOT/fresco-split" && cargo test -q --lib production 2>&1 | tail -n 3) | grep -q "test result: ok" \
    && ok "production policy unit tests" || fail "production policy unit tests"

python3 - "$ROOT/frescod/etc/services.d/40-atrium-frescod.toml" <<'PY' && ok "shipped service runs atrium-frescod as _frescod, no dev settings" || fail "shipped service config"
import sys, tomllib
m = tomllib.load(open(sys.argv[1], "rb"))
e = m["exec"]
assert m.get("enabled", True), "service disabled"
assert e["path"].endswith("/atrium-frescod"), e["path"]
assert e["argv"][0] == "atrium-frescod", e["argv"]
assert e["uid"] == 65010 and e["gid"] == 65010, (e["uid"], e["gid"])
bad = {"FRESCOD_SPLIT", "FRESCOD_RENDERER", "FRESCOD_SPLIT_PIXELS", "FRESCOD_RENDERER_LAUNCHER",
       "FRESCOD_GPU_ISOLATION_PROBE", "FRESCOD_GPU_ONLY", "FRESCOD_RENDERER_BIN"}
for kv in e.get("env", []):
    assert kv["key"] not in bad, f"service sets {kv['key']}={kv['value']}"
PY

if grep -nE 'FRESCOD_(SPLIT|RENDERER|SPLIT_PIXELS|RENDERER_LAUNCHER|GPU_ISOLATION_PROBE|GPU_ONLY)=' \
    "$ROOT/frescod/etc/atrium-frescod" "$ROOT/frescod/etc/services.d/"*.toml \
    "$ROOT/portcullis/portcullisd/src/system_services.rs" "$ROOT/scripts/fresco-renderer-bundle.sh" \
    "$ROOT/scripts/bootstrap-atrium.sh" 2>/dev/null | grep -vE '^[^:]*:[0-9]+:[[:space:]]*(#|//)'; then
    fail "an install or launch file sets a development variable"
fi
ok "install and launch files set no development variables"

if [ "${1:-}" = vm ]; then
    BIN="${2:?usage: $0 vm <frescod path in the VM>}"
    V="$ROOT/scripts/vssh"
    for kv in FRESCOD_SPLIT=0 FRESCOD_RENDERER=vulkan FRESCOD_SPLIT_PIXELS=0 \
              FRESCOD_RENDERER_LAUNCHER=unconfined FRESCOD_GPU_ISOLATION_PROBE=1; do
        rc=$("$V" "d=\$(mktemp -d); ln -s $BIN \$d/atrium-frescod; env $kv FRESCOD_SOCK=\$d/s \$d/atrium-frescod >\$d/log 2>&1; echo \$?; rm -rf \$d")
        [ "$rc" = 78 ] && ok "VM: atrium-frescod with $kv refused (exit 78)" || fail "VM: atrium-frescod with $kv exited $rc"
    done
fi
echo "PASS ($pass checks)"

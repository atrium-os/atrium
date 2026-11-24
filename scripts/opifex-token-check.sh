#!/bin/sh
# Root keys on hardware tokens (docs/spec/opifex.md §5.1, §10.3), rehearsed on
# SoftHSM, a software PKCS#11 token: the key ceremony and a handover to new
# owners, through the real `opifex` CLI with PINs typed at a terminal (a pty
# here). Runs on the HOST: `apt install softhsm2 opensc` or
# `brew install softhsm opensc`. Ends "opifex-token: N ok, M failed" and exits non-zero on a failure.
#
#   - six tokens with a P-256 key each; `opifex token list` sees them and
#     `opifex token ref` writes key files naming them;
#   - `catalog root-new` from three token keys, a catalog and a platform key;
#     two tokens sign it (`root-sign`); a machine's copy syncs it;
#   - a handover: a next root.toml with three new token keys, signed by two of
#     the old and two of the new; the machine adopts it;
#   - a root.toml after that signed by old keys only is refused;
#   - a wrong PIN signs nothing;
#   - a new platform key: the old one stays in root.toml, retired.
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
. "$ROOT/scripts/lib/gate.sh"
gate_begin opifex-token 18
M=""
for m in /usr/lib/softhsm/libsofthsm2.so /usr/lib/x86_64-linux-gnu/softhsm/libsofthsm2.so \
         /usr/local/lib/softhsm/libsofthsm2.so /opt/homebrew/lib/softhsm/libsofthsm2.so; do
    [ -f "$m" ] && { M=$m; break; }
done
[ -n "$M" ] || gate_abort "no SoftHSM: apt install softhsm2 opensc (or brew install softhsm opensc)"
command -v pkcs11-tool >/dev/null || gate_abort "no pkcs11-tool (opensc)"

echo "=== build"
(cd "$ROOT/portcullis" && cargo build -q --bin opifex) || gate_abort "opifex did not build"
O="${CARGO_TARGET_DIR:-$ROOT/portcullis/target}/debug/opifex"
W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT
cd "$W"
mkdir tokens
printf 'directories.tokendir = %s/tokens\nobjectstore.backend = file\nlog.level = ERROR\n' "$W" > softhsm2.conf
export SOFTHSM2_CONF=$W/softhsm2.conf

# Run a command whose PIN prompts are answered with $1 through a pty.
cat > answer-pin.py <<'PY'
import os, pty, sys
pin = sys.argv[1].encode()
pid, fd = pty.fork()
if pid == 0:
    os.execvp(sys.argv[2], sys.argv[2:])
buf = b""
while True:
    try:
        d = os.read(fd, 1024)
    except OSError:
        break
    if not d:
        break
    buf += d
    if buf.rstrip().endswith(b":") and b"PIN for" in buf.splitlines()[-1]:
        os.write(fd, pin + b"\n")
        buf += b"\n"
sys.stdout.write(buf.decode(errors="replace"))
_, st = os.waitpid(pid, 0)
sys.exit(os.waitstatus_to_exitcode(st))
PY
P() { python3 -I answer-pin.py "$@"; }

echo "=== six tokens"
for i in 1 2 3 4 5 6; do
    softhsm2-util --init-token --free --label root$i --pin 123456 --so-pin 87654321 >/dev/null
    pkcs11-tool --module "$M" --token-label root$i --login --pin 123456 \
        --keypairgen --key-type EC:prime256v1 --id 01 --label root$i >/dev/null 2>&1
done
L=$("$O" token list --module "$M")
check "token list sees six tokens with a key" "6" "$(echo "$L" | grep -c '^  id 01  root')"
for i in 1 2 3 4 5 6; do
    s=$(echo "$L" | sed -n "s/^token \"root$i\" serial \([^ ]*\) .*/\1/p")
    "$O" token ref --module "$M" --serial "$s" --id 01 --out r$i.key --pub r$i.pub >/dev/null
done
check "a key file names the token, not the key" "0" "$(grep -c 'PRIVATE' r1.key)"
check "token pub matches the ref's public key" "yes" "$("$O" token pub r1.key | cmp -s - r1.pub && echo yes)"
openssl ecparam -name prime256v1 -genkey -noout | openssl pkcs8 -topk8 -nocrypt -out catalog.pem
openssl ecparam -name prime256v1 -genkey -noout | openssl pkcs8 -topk8 -nocrypt -out platform.pem

echo "=== the ceremony"
"$O" catalog root-new site --name Atrium --root r1.key --root r2.key --root r3.key \
    --catalog catalog.pem --platform platform.pem >/dev/null
check "root-new writes an unsigned root.toml" "0" "$(wc -c < site/root.toml.sig | tr -d ' ')"
check "root-new refuses to overwrite" "1" "$("$O" catalog root-new site --name Atrium --root r1.key --catalog catalog.pem 2>&1 | grep -c 'already')"
check "a wrong PIN signs nothing" "1" "$(P 000000 "$O" catalog root-sign site --key r1.key 2>&1 | grep -c 'wrong PIN')"
check "nothing was added" "0" "$(wc -c < site/root.toml.sig | tr -d ' ')"
P 123456 "$O" catalog root-sign site --key r1.key >/dev/null
P 123456 "$O" catalog root-sign site --key r2.key >/dev/null
check "two token signatures" "2" "$(wc -l < site/root.toml.sig | tr -d ' ')"
"$O" catalog head site --key catalog.pem >/dev/null
check "a machine takes the signed root" "1" "$("$O" catalog sync state --from site --root site/root.toml 2>&1 | grep -c 'checked')"
check "the platform key may publish every kind" "1" "$(grep -c 'kinds = \["app", "service", "layer", "font", "driver", "firmware"\]' site/root.toml)"

echo "=== handover to new owners"
"$O" catalog root-new site --next yes --name Atrium --root r4.key --root r5.key --root r6.key \
    --catalog catalog.pem --platform platform.pem >/dev/null
check "the next root is version 2" "1" "$(grep -c '^version = 2' site/root.toml)"
for k in r1 r2 r4 r5; do P 123456 "$O" catalog root-sign site --key $k.key >/dev/null; done
"$O" catalog head site --key catalog.pem >/dev/null
check "the machine adopts the new owners' root" "1" "$("$O" catalog sync state --from site 2>&1 | grep -c 'checked')"
check "its pinned root is version 2" "1" "$(grep -c '^version = 2' state/root.toml)"

echo "=== the old owners alone can't change it any more"
"$O" catalog root-new site --next yes --name Atrium --root r1.key --root r2.key --root r3.key \
    --catalog catalog.pem --platform platform.pem >/dev/null
for k in r1 r2 r3; do P 123456 "$O" catalog root-sign site --key $k.key >/dev/null; done
"$O" catalog head site --key catalog.pem >/dev/null
check "a root signed by the old keys only is refused" "1" "$("$O" catalog sync state --from site 2>&1 | grep -c 'not signed by 2 of the current root keys')"
check "the machine keeps version 2" "1" "$(grep -c '^version = 2' state/root.toml)"

echo "=== a lost platform key is retired, not dropped"
openssl ecparam -name prime256v1 -genkey -noout | openssl pkcs8 -topk8 -nocrypt -out platform2.pem
"$O" catalog root-new site --next yes --name Atrium --root r4.key --root r5.key --root r6.key \
    --catalog catalog.pem --platform platform2.pem >/dev/null
check "the old platform key stays, retired at the log's end" "1" "$(grep -c '^until = 0' site/root.toml)"
for k in r4 r5; do P 123456 "$O" catalog root-sign site --key $k.key >/dev/null; done
"$O" catalog head site --key catalog.pem >/dev/null
check "the machine adopts it" "1" "$("$O" catalog sync state --from site 2>&1 | grep -c 'checked')"
check "a retired key never comes back" "1" "$("$O" catalog root-new site --next yes --name Atrium --root r4.key --root r5.key --root r6.key \
    --catalog catalog.pem --platform platform.pem 2>&1 | grep -c 'never comes back')"

gate_end

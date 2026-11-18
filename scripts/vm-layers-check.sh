#!/bin/sh
# Toolchain layers (dev-tools-plan.md M3e) in the amd64 cloud VM: the c-dev
# and Rust layers, signed with a gate key standing in for the platform key,
# install with opifex and are composed into workspaces by portcullisd. Runs
# on the HOST after cloud-vm.sh up and the open-files gate's users (alice).
# Ends "layers: N failed".
#
#   sh scripts/vm-layers-check.sh
#
# Needs the layer trees from build-c-dev-layer.sh and build-rust-layer.sh
# (CDEV=<tree>, RUST=<tree>; defaults under ~/work).
#
#   - refused: a layer signed by another key, unsigned, or with a file
#     changed after signing; installed: c-dev and rust;
#   - a Rust workspace (no network) gets both, read-only, on PATH:
#     cargo new, cargo test, clippy and rustfmt work; rust-analyzer answers
#     initialize and a hover on println!; std for aarch64 is there too;
#   - a C workspace: CMake + Ninja and Meson builds run; pkg-config finds
#     the base system's zlib; git commits;
#   - the layer is the same tree in both workspaces (one nullfs source).
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
export ARCH=amd64
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { "$ROOT/scripts/vssh" "$@"; }
fails=0
ok() { echo "  ok $1"; }
bad() { echo "  FAIL $1"; fails=$((fails+1)); }
check() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1: expected [$2] got [$3]"; fi; }
die() { bad "$1"; echo "layers: $fails failed"; exit 1; }
CDEV=${CDEV:-$HOME/work/cdev/c-dev-1-amd64}
RUST=${RUST:-$(ls -d "$HOME"/work/rust-layer/layer/rust-*-amd64 2>/dev/null | tail -1)}
[ -f "$CDEV/atrium.toml" ] || die "no c-dev tree at $CDEV"
[ -n "$RUST" ] && [ -f "$RUST/bin/rustc" ] || die "no Rust layer tree (RUST=)"
RVER=$(basename "$RUST" | sed 's/^rust-\(.*\)-amd64$/\1/')
H=$(mktemp -d)
trap 'rm -rf "$H"' EXIT

echo "=== build (host opifex, VM portcullis)"
(cd "$ROOT/portcullis" && cargo build -q -p opifex) || die "host opifex"
HOPIFEX=$ROOT/portcullis/target/debug/opifex
(cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" \
    -p portcullisd -p portcullis-cli -p opifex --bin portcullisd --bin portcullis --bin opifex) || die "cross-build"

echo "=== sign the layers with a gate key"
openssl ecparam -name prime256v1 -genkey -noout | openssl pkcs8 -topk8 -nocrypt -out "$H/gate.key"
openssl ec -in "$H/gate.key" -pubout -out "$H/gate.pem" 2>/dev/null
openssl ecparam -name prime256v1 -genkey -noout | openssl pkcs8 -topk8 -nocrypt -out "$H/other.key"
# c-dev keeps its manifest; Rust's comes from pack-rust-layer.sh's template.
grep -v '^content = ' "$CDEV/atrium.toml" > "$H/c-dev.toml"
"$HOPIFEX" layer pack "$CDEV" --manifest "$H/c-dev.toml" --sign "$H/gate.key" >/dev/null || die "pack c-dev"
cat > "$H/rust.toml" <<T
kind = "layer"

[layer]
name = "rust"
version = "$RVER"
arch = "amd64"
mount = "/atrium/toolchains/rust"
path = ["bin"]
summary = "Rust $RVER"

[layer.env]
RUST_SRC_PATH = "\${LAYER}/lib/rustlib/src/rust/library"
T
"$HOPIFEX" layer pack "$RUST" --manifest "$H/rust.toml" --sign "$H/gate.key" >/dev/null || die "pack rust"
# Three small layers for the refusals.
for t in other unsigned tampered; do
    mkdir -p "$H/$t/bin"; printf '#!/bin/sh\necho %s\n' "$t" > "$H/$t/bin/$t"; chmod 755 "$H/$t/bin/$t"
    printf 'kind = "layer"\n[layer]\nname = "%s"\nversion = "1"\narch = "amd64"\nmount = "/atrium/toolchains/%s"\npath = ["bin"]\n' $t $t > "$H/$t.toml"
done
"$HOPIFEX" layer pack "$H/other" --manifest "$H/other.toml" --sign "$H/other.key" >/dev/null
"$HOPIFEX" layer pack "$H/unsigned" --manifest "$H/unsigned.toml" >/dev/null
"$HOPIFEX" layer pack "$H/tampered" --manifest "$H/tampered.toml" --sign "$H/gate.key" >/dev/null
echo 'echo changed' >> "$H/tampered/bin/tampered"

echo "=== copy"
B=/var/tmp/layers
V "pkill -x portcullisd; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B"
(cd "$(dirname "$CDEV")" && tar -cf - "$(basename "$CDEV")" | zstd -q -3 -T0 > "$H/c-dev.tar.zst")
(cd "$(dirname "$RUST")" && tar -cf - "$(basename "$RUST")" | zstd -q -3 -T0 > "$H/rust.tar.zst")
(cd "$H" && tar -cf small.tar other unsigned tampered gate.pem)
for f in c-dev.tar.zst rust.tar.zst small.tar; do
    want=$(sha256sum "$H/$f" | cut -d' ' -f1)
    scp -q $K -P 2222 "$H/$f" root@localhost:$B/ || die "copy $f"
    got=$(V "sha256 -q $B/$f")
    [ "$want" = "$got" ] || die "$f arrived damaged (TCG scp); run again"
done
for b in portcullisd portcullis opifex; do
    scp -q $K -P 2222 "$ROOT/portcullis/target/$TARGET/release/$b" root@localhost:$B/ || die "copy $b"
done
V "cd $B && tar -xf c-dev.tar.zst && tar -xf rust.tar.zst && tar -xf small.tar && rm -f *.tar.zst" || die "unpack"
ok "layers copied ($(du -sh "$H/c-dev.tar.zst" "$H/rust.tar.zst" | cut -f1 | tr '\n' ' '))"

echo "=== install"
V 'sh -s' <<EOS
set -e
install -m 0755 $B/portcullisd /usr/local/bin/portcullisd
install -m 0755 $B/portcullis /usr/local/bin/portcullis
install -m 0755 $B/opifex /usr/local/bin/opifex
mkdir -p /etc/atrium/platform-keys
cp $B/gate.pem /etc/atrium/platform-keys/layers-gate.pem
for l in \$(opifex layer list 2>/dev/null | awk '{print \$1}' | grep @); do opifex layer remove \$l >/dev/null 2>&1 || true; done
EOS
refused() { # name dir why-pattern
    out=$(V "opifex layer install $B/$2 2>&1")
    if V "opifex layer list" | grep -q " $2@"; then bad "$1 was installed: $out"
    else case "$out" in *$3*) ok "$1 is refused" ;; *) ok "$1 is refused ($out)" ;; esac; fi
}
refused "a layer signed by another key" other "platform key"
refused "an unsigned layer" unsigned "not signed"
refused "a layer with a file changed after signing" tampered "index"
out=$(V "opifex layer install $B/$(basename "$CDEV") 2>&1") && ok "c-dev installs" || bad "c-dev: $out"
out=$(V "opifex layer install $B/$(basename "$RUST") 2>&1") && ok "rust installs" || bad "rust: $out"
L=$(V "opifex layer list")
echo "$L" | sed 's/^/    /'
echo "$L" | grep -q " rust@$RVER " && echo "$L" | grep -q " c-dev@1 " && ok "both listed" || bad "list: $L"
check "installed trees are root's, not writable by others" "root 755" "$(V "stat -f '%Su %Lp' /var/lib/atrium/layers/rust@$RVER")"

echo "=== portcullisd"
V "id alice >/dev/null 2>&1 || pw useradd alice -u 1011 -m -d /home/alice -s /bin/sh"
PSOCK=/atrium/sockets/portcullis/portcullis.sock
LOG=$B/portcullisd.log
V "mkdir -p /atrium/sockets/portcullis; rm -f $PSOCK; daemon -f -o $LOG /usr/local/bin/portcullisd --socket $PSOCK; sleep 3; chmod 666 $PSOCK; test -S $PSOCK" \
    && ok "portcullisd is up" || die "portcullisd: $(V "tail -3 $LOG")"

W=/home/alice/.atrium/workspaces
ws() { V "su -m alice -c 'portcullis workspace $1'" 2>&1; }
# x <workspace> <command>: run in the workspace (in work), output on
# stdout. The command goes over as a file so no shell on the way expands it.
x() {
    printf '%s\n' "$2" > "$H/cmd.sh"
    scp -q $K -P 2222 "$H/cmd.sh" "root@localhost:$W/$1/work/.gate.sh" && V "chown alice:files-alice $W/$1/work/.gate.sh"
    V "su -m alice -c 'portcullis workspace exec $1 -- sh .gate.sh'" 2>&1
}
for w in rs cw; do ws "stop $w" >/dev/null; done
V "for j in \$(jls name | grep -E '^ws_alice_(rs|cw)\$'); do jail -r \$j; done; rm -rf $W/rs $W/cw /var/lib/atrium/workspaces/alice/rs /var/lib/atrium/workspaces/alice/cw"

echo "=== a Rust workspace"
out=$(ws "new rs --toolchain rust --network none")
echo "$out" | grep -q "^rs" && ok "made (rust, no network)" || bad "new rs: $out"
check "its project file names both layers" "1" "$(V "grep -c 'layers = \[\"rust@$RVER\", \"c-dev@1\"\]' $W/rs/work/.atrium/devjail.toml" | tr -d ' ')"
out=$(x rs 'echo PATH=$PATH; for c in cargo rustc rust-analyzer clangd; do command -v $c; done')
echo "$out" | grep -q "PATH=\(/atrium/bin:\)\{0,1\}/atrium/toolchains/rust/bin:/atrium/toolchains/c-dev/bin:" && ok "layers on PATH before the base" || bad "PATH: $out"
echo "$out" | grep -q "/atrium/toolchains/rust/bin/rust-analyzer" && ok "rust-analyzer by bare name" || bad "rust-analyzer: $out"
out=$(x rs 'touch /atrium/toolchains/rust/bin/x 2>&1; echo rc=$?')
echo "$out" | grep -q "rc=1" && ok "the layer is read-only" || bad "layer write: $out"
out=$(x rs 'rustc --version; cargo --version')
echo "$out" | grep -q "rustc $RVER" && ok "$(echo "$out" | head -1)" || bad "rustc: $out"
out=$(x rs 'cargo new -q hello && cd hello && cargo test -q 2>&1 | tail -3 && cargo run -q')
echo "$out" | grep -q "Hello, world!" && ok "cargo new, test, run (no network)" || bad "cargo: $out"
out=$(x rs 'cd hello && cargo clippy -q -- -D warnings 2>&1; echo clippy=$?; cargo fmt --check; echo fmt=$?')
echo "$out" | grep -q "clippy=0" && ok "clippy" || bad "clippy: $out"
echo "$out" | grep -q "fmt=0" && ok "rustfmt" || bad "rustfmt: $out"
out=$(x rs 'ls $(rustc --print target-libdir --target aarch64-unknown-freebsd) | grep -c "^libstd-.*rlib$"')
check "std for aarch64 is in the layer" "1" "$(echo "$out" | tail -1)"
check "RUST_SRC_PATH points at the std source" "1" "$(x rs 'test -f $RUST_SRC_PATH/std/src/lib.rs && echo 1' | tail -1)"

echo "=== rust-analyzer (initialize, hover; slow under TCG)"
cat > "$H/lsp.py" <<'PY'
import json, os, subprocess, sys, time
p = subprocess.Popen(["rust-analyzer"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
def send(m):
    b = json.dumps(m).encode()
    p.stdin.write(b"Content-Length: %d\r\n\r\n" % len(b) + b); p.stdin.flush()
def recv():
    n = 0
    while True:
        l = p.stdout.readline()
        if not l: sys.exit("rust-analyzer exited")
        l = l.strip()
        if not l: break
        if l.lower().startswith(b"content-length:"): n = int(l.split(b":")[1])
    return json.loads(p.stdout.read(n))
def call(i, method, params):
    send({"jsonrpc": "2.0", "id": i, "method": method, "params": params})
    while True:
        m = recv()
        if m.get("id") == i and "method" not in m: return m
root = os.path.abspath("hello"); uri = "file://" + root + "/src/main.rs"
r = call(1, "initialize", {"processId": os.getpid(), "rootUri": "file://" + root, "capabilities": {}})
print("initialize:", r["result"]["serverInfo"]["name"], r["result"]["serverInfo"].get("version", ""))
send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
send({"jsonrpc": "2.0", "method": "textDocument/didOpen", "params": {"textDocument": {"uri": uri, "languageId": "rust", "version": 1, "text": open(root + "/src/main.rs").read()}}})
end = time.time() + int(sys.argv[1]); i = 2
while time.time() < end:
    r = call(i, "textDocument/hover", {"textDocument": {"uri": uri}, "position": {"line": 1, "character": 6}}); i += 1
    v = (r.get("result") or {}).get("contents", {})
    t = v.get("value", "") if isinstance(v, dict) else str(v)
    if "println" in t:
        print("hover:", " ".join(t.split())[:120]); break
    time.sleep(5)
else:
    print("hover: nothing in time")
p.kill()
PY
scp -q $K -P 2222 "$H/lsp.py" root@localhost:$W/rs/work/lsp.py && V "chown alice:files-alice $W/rs/work/lsp.py"
out=$(x rs 'python3.12 lsp.py 1500')
echo "$out" | sed 's/^/    /'
echo "$out" | grep -q "^initialize: rust-analyzer" && ok "rust-analyzer initializes" || bad "initialize: $out"
echo "$out" | grep -q "^hover:.*println" && ok "hover on println! answers" || bad "hover: $out"

echo "=== a C workspace"
out=$(ws "new cw --toolchain c --network none")
echo "$out" | grep -q "^cw" && ok "made (c, no network)" || bad "new cw: $out"
put() { printf '%b' "$2" > "$H/f"; scp -q $K -P 2222 "$H/f" "root@localhost:$W/cw/work/$1"; V "chown alice:files-alice $W/cw/work/$1"; }
V "mkdir -p $W/cw/work/cm $W/cw/work/me; chown alice:files-alice $W/cw/work/cm $W/cw/work/me; chmod 2775 $W/cw/work/cm $W/cw/work/me"
put cm/CMakeLists.txt 'cmake_minimum_required(VERSION 3.20)\nproject(cm C)\nadd_executable(cm cm.c)\n'
put cm/cm.c '#include <stdio.h>\nint main(void) { puts("hello from cmake"); return 0; }\n'
put me/meson.build "project('me', 'c')\nexecutable('me', 'me.c', dependencies: dependency('zlib'))\n"
put me/me.c '#include <stdio.h>\n#include <zlib.h>\nint main(void) { fputs("zlib ", stdout); puts(zlibVersion()); return 0; }\n'
out=$(x cw 'cd cm && cmake -S . -B b -G Ninja >/dev/null && cmake --build b >/dev/null && ./b/cm')
check "CMake + Ninja" "hello from cmake" "$(echo "$out" | tail -1)"
out=$(x cw 'cd me && meson setup b >/dev/null && meson compile -C b >/dev/null && ./b/me')
echo "$out" | tail -1 | grep -q "^zlib " && ok "Meson with zlib ($(echo "$out" | tail -1))" || bad "meson: $out"
out=$(x cw 'pkg-config --libs zlib' | tail -1)
case " $out " in *" -lz "*) ok "pkg-config finds the base system's zlib ($out)" ;; *) bad "pkg-config: $out" ;; esac
out=$(x cw 'git init -q g && cd g && echo a > a && git add a && git -c user.name=t -c user.email=t@t commit -qm one && git log --oneline | wc -l')
check "git commits" "1" "$(echo "$out" | tail -1 | tr -d ' ')"
out=$(x cw 'command -v rustc || echo none')
check "a C workspace doesn't get Rust" "none" "$(echo "$out" | tail -1)"

echo "=== shared"
m=$(V "mount -p | awk '\$2 ~ /toolchains\/c-dev\$/ {print \$1}'")
n=$(echo "$m" | grep -c .); u=$(echo "$m" | sort -u | grep -c .)
[ "$n" -ge 2 ] && [ "$u" = 1 ] && ok "both workspaces mount the same c-dev tree ($(echo "$m" | head -1))" || bad "c-dev mounts: $(V "mount -p | grep toolchains")"

for w in rs cw; do ws "stop $w" >/dev/null; done
V "pkill -x portcullisd"
echo "layers: $fails failed"
[ $fails = 0 ]

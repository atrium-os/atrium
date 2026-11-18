#!/bin/sh
# Dev-tools M7 (docs/spec/dev-tools-plan.md §8, dev-tools.md §4.5): an app
# made with cargo-atrium from new to an installed, signed bundle. Runs on the
# HOST after
#   ARCH=amd64 sh scripts/cloud-vm.sh up
#   sh scripts/vm-bundle-check.sh
# It installs the current opifex, atrium-jaild, portcullisd and portcullis.
# Ends "bundle: N failed". Passed 8/8 on 2026-11-18 in a cloud amd64 VM
# (GENERIC kernel).
#
#   - `cargo atrium new` (app template), two library functions moved into
#     workers with `cargo atrium worker add --from` (one fd, one message);
#   - `cargo atrium bundle --sign` cross-builds it against the VM's sysroot
#     and writes the tree (programs, the libraries they need, the loader),
#     packs, signs and archives it to an .insula;
#   - in the VM the .insula unpacks and installs through the trust gate with
#     the publisher's key, with nothing left for install to resolve;
#   - launched, its core calls both workers, each in a jail of its own, and
#     prints what came back; no worker jail is left afterwards.
# No frescod: the core is a probe that opens no window, and the manifest's
# graphics line is dropped.
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
die() { echo "  FAIL $1"; echo "bundle: $((fails+1)) failed"; exit 1; }
G=${GATE_DIR:-$HOME/work/bundle-gate}
APP=org.test.bundledemo

echo "=== a project: new, two workers moved in"
(cd "$ROOT/sdk" && cargo build -q -p cargo-atrium) || die "cargo-atrium build"
(cd "$ROOT/portcullis" && cargo build -q -p opifex) || die "host opifex build"
CA="$ROOT/sdk/target/debug/cargo-atrium"
export OPIFEX="$ROOT/portcullis/target/debug/opifex"
rm -rf "$G/bundle-demo" "$G/out"; mkdir -p "$G"
(cd "$G" && "$CA" atrium new bundle-demo --sdk-path "$ROOT" --id $APP >/dev/null) || die "new"
P="$G/bundle-demo"
sed -i.bak -e '/^graphics = /d' "$P/atrium.toml" && rm -f "$P/atrium.toml.bak"
cat > "$P/src/lib.rs" <<'T'
//! What the core and its workers share.

pub mod parse {
    /// A line count; refuses what starts with BAD.
    pub fn lines(bytes: &[u8]) -> Result<usize, String> {
        if bytes.starts_with(b"BAD") {
            return Err("bad input".into());
        }
        Ok(bytes.iter().filter(|b| **b == b'\n').count())
    }
    pub fn shout(text: &str, times: u32) -> String {
        text.to_uppercase().repeat(times.min(16) as usize)
    }
}
T
cat > "$P/src/main.rs" <<'T'
//! The probe core: the workers' calls, without a window.
fn say(s: String) { println!("bundle-gate {s}") }
fn main() {
    let root = std::path::PathBuf::from(std::env::var_os("ATRIUM_APP_ROOT").unwrap_or_else(|| "/".into()));
    match std::fs::File::open(root.join("atrium.toml")).map_err(|e| e.to_string()).and_then(bundle_demo::count_worker::call) {
        Ok(n) => say(format!("count {n}")),
        Err(e) => say(format!("count error {e}")),
    }
    match bundle_demo::shout_worker::call("hi", 2) {
        Ok(s) => say(format!("shout {s}")),
        Err(e) => say(format!("shout error {e}")),
    }
    say("done".into());
}
T
(cd "$P" && "$CA" atrium worker add count --from parse::lines >/dev/null && "$CA" atrium worker add shout --input message --from parse::shout >/dev/null) \
    || die "worker add"
ok "worker add: count (fd) and shout (message)"

echo "=== bundle"
openssl ecparam -name prime256v1 -genkey -noout -out "$G/key.pem" 2>/dev/null
openssl pkcs8 -topk8 -nocrypt -in "$G/key.pem" -out "$G/key8.pem"
openssl ec -in "$G/key.pem" -pubout -out "$G/pub.pem" 2>/dev/null
T="$G/out/$APP"
# The VM's sysroot has the libraries but not the loader; take the VM's own.
[ -f "$VMDIR/sysroot/libexec/ld-elf.so.1" ] || { mkdir -p "$VMDIR/sysroot/libexec" &&
    scp -q $K -P 2222 root@localhost:/libexec/ld-elf.so.1 "$VMDIR/sysroot/libexec/"; } || die "no loader for the sysroot"
(cd "$P" && RUSTFLAGS= "$CA" atrium bundle --sysroot "$VMDIR/sysroot" --sign "$G/key8.pem" --out "$T" -- -q >"$G/bundle.log" 2>&1) \
    || die "bundle: $(tail -5 "$G/bundle.log")"
for f in bin/bundle-demo bin/bundle-demo-count bin/bundle-demo-shout lib/libc.so.7 lib/libthr.so.3 libexec/ld-elf.so.1 content.index atrium.toml.sig; do
    [ -f "$T/$f" ] || bad "no $f in the bundle"
done
[ -f "$T.insula" ] && ok "bundle: the programs, their libraries and the loader, signed, $(du -h "$T.insula" | cut -f1) .insula" || bad "no .insula"

echo "=== install"
B=/var/tmp/bundle-gate
V "pkill -x portcullisd; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B"
(cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" \
    -p portcullisd -p portcullis-cli -p jaild -p opifex --bin portcullisd --bin portcullis --bin atrium-jaild --bin opifex) \
    || die "cross-build"
for f in "$ROOT/portcullis/target/$TARGET/release/portcullisd" "$ROOT/portcullis/target/$TARGET/release/portcullis" \
         "$ROOT/portcullis/target/$TARGET/release/atrium-jaild" "$ROOT/portcullis/target/$TARGET/release/opifex" \
         "$T.insula" "$G/pub.pem" "$ROOT/etc/jaild.policy.toml"; do
    scp -q $K -P 2222 "$f" root@localhost:$B/ || die "copy $f"
done
PSOCK=/atrium/sockets/portcullis/portcullis.sock
LOG=$B/portcullisd.log
V 'sh -s' <<EOS
set -e
install -m 0755 $B/portcullisd /usr/local/bin/portcullisd
install -m 0755 $B/portcullis /usr/local/bin/portcullis
install -m 0755 $B/opifex /usr/local/bin/opifex
service atrium-jaild stop >/dev/null 2>&1 || true
pkill -x atrium-jaild || true; sleep 1; rm -f /var/run/atrium/jaild.sock
install -m 0755 $B/atrium-jaild /usr/local/bin/atrium-jaild
mkdir -p /etc/atrium/publishers
install -m 0644 $B/pub.pem /etc/atrium/publishers/bundle-gate.pem
sed -e 's/^mode     = 1/mode     = 2/' $B/jaild.policy.toml > /etc/atrium/jaild.policy.toml
grep -q '^allow_rctl' /etc/atrium/jaild.policy.toml || printf '\n[resource_control]\nallow_rctl = true\n' >> /etc/atrium/jaild.policy.toml
id alice >/dev/null 2>&1 || pw useradd alice -m -s /bin/sh
service atrium-jaild start >/dev/null 2>&1 || true
i=0; while [ \$i -lt 30 ] && ! test -S /var/run/atrium/jaild.sock; do sleep 1; i=\$((i+1)); done; test -S /var/run/atrium/jaild.sock
mkdir -p /atrium/sockets/portcullis; rm -f $PSOCK
: > $LOG
daemon -f -o $LOG /usr/local/bin/portcullisd --socket $PSOCK
sleep 3; test -S $PSOCK; chmod 666 $PSOCK
test -S /atrium/sockets/workers/workers.sock
EOS
[ $? = 0 ] && ok "portcullisd up, with the worker-call socket" || die "setup: $(V "tail -3 $LOG")"
out=$(V "opifex uninstall $APP >/dev/null 2>&1; cd $B && opifex unarchive $APP.insula tree >/dev/null && opifex install $B/tree 2>&1")
case "$out" in *"installed $APP"*"0 runtime file(s) resolved"*) ok "the .insula installs, signed, with its closure: nothing to resolve" ;;
               *) bad "install: $out" ;; esac
APPS=/var/lib/atrium/apps
r=$(V "cmp -s $B/tree/lib/libc.so.7 $APPS/$APP/lib/libc.so.7 && echo same")
[ "$r" = same ] && ok "its own libc is what's installed" || bad "libc: $r"

echo "=== launch"
V ": > $B/app.log; daemon -f -o $B/app.log su -m alice -c 'portcullis launch --no-prompt $APP'"
line() { V "cat $B/app.log $LOG | grep 'bundle-gate $1 ' | tail -1" | sed "s/.*bundle-gate $1 //"; }
i=0; while [ $i -lt 120 ] && ! V "cat $B/app.log $LOG | grep -q 'bundle-gate done'"; do sleep 3; i=$((i+3)); done
want=$(V "grep -c '' $APPS/$APP/atrium.toml" | tr -d ' ')
r=$(line count); [ "$r" = "$want" ] && ok "count: the fd worker counted the manifest's $r lines" || bad "count: [$r] want $want ($(V "tail -5 $B/app.log; tail -5 $LOG"))"
r=$(line shout); [ "$r" = HIHI ] && ok "shout: the message worker answered $r" || bad "shout: [$r]"
sleep 3
left=$(V "jls name | grep -c 'bundledemo.*w-'" | tr -d ' ')
[ "$left" = 0 ] && ok "no worker jail left" || bad "$left worker jails left"
V "pkill -f 'portcullis launch --no-prompt $APP'; opifex uninstall $APP >/dev/null 2>&1; rm -f /etc/atrium/publishers/bundle-gate.pem"
echo "bundle: $fails failed"
[ $fails = 0 ]

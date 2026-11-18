#!/bin/sh
# Dev-tools M7 (docs/spec/dev-tools-plan.md §8): the viewer and indexer
# templates' workers, jailed. Runs on the HOST after
#   ARCH=amd64 sh scripts/cloud-vm.sh up      (kernel.atrium for the filter)
#   sh scripts/vm-templates-check.sh
# It installs the current atrium-jaild, portcullisd and portcullis. Passed
# 10/10 on 2026-11-18 in a cloud amd64 VM on FreeBSD's GENERIC kernel (no
# kernel.atrium in that image, so the filter's deny mode wasn't in force).
# Ends "templates: N failed". The filter denies (mode 2). No frescod, so
# each project's window core is swapped for a probe core that makes the
# same calls and prints what came back (and the manifest's graphics line
# is dropped) to portcullisd's log:
#   - viewer: the decode worker (input fd, a jail per call) turns the
#     template's seed picture into a whole Picture, scaled to max_side,
#     and answers a file that isn't a BMP with its own error;
#   - indexer: one index worker (input message, lifetime session) keeps
#     its counts from one call to the next;
#   - and after it all no worker jail is left.
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
G=${GATE_DIR:-$HOME/work/templates-gate}

echo "=== projects from the templates, and a probe core"
(cd "$ROOT/sdk" && cargo build -q -p cargo-atrium) || { echo "templates: 1 failed"; exit 1; }
CA="$ROOT/sdk/target/debug/cargo-atrium"
rm -rf "$G/my-viewer" "$G/my-indexer" "$G/probe/src"; mkdir -p "$G/probe/src"
(cd "$G" && "$CA" atrium new my-viewer --template viewer --sdk-path "$ROOT" >/dev/null && "$CA" atrium new my-indexer --template indexer --sdk-path "$ROOT" >/dev/null) \
    || { echo "templates: 1 failed"; exit 1; }
cat > "$G/probe/Cargo.toml" <<T
[package]
name = "probe"
version = "0.0.0"
edition = "2021"
[dependencies]
atrium-worker = { path = "$ROOT/sdk/atrium-worker" }
libc = "0.2"
my-viewer = { path = "$G/my-viewer" }
my-indexer = { path = "$G/my-indexer" }
[workspace]
T
cat > "$G/probe/src/main.rs" <<'T'
//! The templates' calls, without a window.
use std::fs::File;
use std::path::PathBuf;
use atrium_worker::{Error, Workers};
use std::io::{Seek, Write};
use std::os::fd::FromRawFd;

fn say(s: String) { println!("templates-gate {s}") }

/// The seed picture as a file of its own (a Run's bundle has only the
/// entries and the manifest): anonymous shared memory.
fn seed_file() -> File {
    let fd = unsafe { libc::shm_open(libc::SHM_ANON, libc::O_RDWR, 0o600) };
    assert!(fd >= 0, "shm_open");
    let bytes = include_bytes!("../../my-viewer/fuzz/corpus/worker-decode/seed.bmp");
    assert_eq!(unsafe { libc::ftruncate(fd, bytes.len() as libc::off_t) }, 0, "ftruncate");
    let mut f = unsafe { File::from_raw_fd(fd) };
    f.write_all(bytes).unwrap();
    f.rewind().unwrap();
    f
}

fn main() {
    let root = PathBuf::from(std::env::var_os("ATRIUM_APP_ROOT").unwrap_or_else(|| "/".into()));
    let w = match Workers::get() { Ok(w) => w, Err(e) => return say(format!("error {e}")) };
    if w.defs().any(|d| d.name == "decode") {
        let seed = seed_file();
        match w.call::<_, my_viewer::Picture>("decode", &my_viewer::Ask { max_side: 4096 }, Some(seed)) {
            Ok(p) => say(format!("viewer picture {}x{} whole={} first={:?}", p.width, p.height, p.is_whole(), &p.rgba[..4])),
            Err(e) => say(format!("viewer picture error {e}")),
        }
        let seed = seed_file();
        match w.call::<_, my_viewer::Picture>("decode", &my_viewer::Ask { max_side: 1 }, Some(seed)) {
            Ok(p) => say(format!("viewer small {}x{}", p.width, p.height)),
            Err(e) => say(format!("viewer small error {e}")),
        }
        let other = File::open(root.join("atrium.toml")).expect("atrium.toml");
        match w.call::<_, my_viewer::Picture>("decode", &my_viewer::Ask { max_side: 64 }, Some(other)) {
            Err(Error::Worker(m)) => say(format!("viewer other worker-error {m}")),
            r => say(format!("viewer other unexpected {:?}", r.map(|p| (p.width, p.height)))),
        }
        say("viewer done".into());
    } else {
        let mut s = match w.session("index") { Ok(s) => s, Err(e) => return say(format!("indexer error {e}")) };
        let a = s.call::<_, my_indexer::Top>(&my_indexer::Add { text: "a b a".into() }, None);
        let b = s.call::<_, my_indexer::Top>(&my_indexer::Add { text: "a c".into() }, None);
        match (a, b) {
            (Ok(_), Ok(t)) => say(format!("indexer counts total={} top={}:{} file={}", t.total, t.words[0].word, t.words[0].count, t.file.len())),
            (a, b) => say(format!("indexer error {:?} {:?}", a.err(), b.err())),
        }
        say("indexer done".into());
    }
}
T

echo "=== cross-build"
# With the SDK's own cross build, against the VM's sysroot.
X() { "$CA" atrium cross amd64 --sysroot "$VMDIR/sysroot" -q --release "$@" >/dev/null; }
for p in my-viewer:my-viewer-decode my-indexer:my-indexer-index; do
    (cd "$G/${p%%:*}" && X --bin "${p#*:}") || { echo "build ${p#*:} failed"; echo "templates: 1 failed"; exit 1; }
done
(cd "$G/probe" && X) || { echo "probe build failed"; echo "templates: 1 failed"; exit 1; }
ok "the workers and the probe build with cargo atrium cross"

echo "=== install"
B=/var/tmp/templates-gate
V "pkill -x portcullisd; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B"
(cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" \
    -p portcullisd -p portcullis-cli -p jaild --bin portcullisd --bin portcullis --bin atrium-jaild) \
    || { echo "build failed"; echo "templates: 1 failed"; exit 1; }
for f in "$ROOT/portcullis/target/$TARGET/release/portcullisd" "$ROOT/portcullis/target/$TARGET/release/portcullis" \
         "$ROOT/portcullis/target/$TARGET/release/atrium-jaild" \
         "$G/my-viewer/target/$TARGET/release/my-viewer-decode" "$G/my-indexer/target/$TARGET/release/my-indexer-index" \
         "$G/probe/target/$TARGET/release/probe" \
         "$G/my-viewer/atrium.toml" "$ROOT/etc/jaild.policy.toml"; do
    scp -q $K -P 2222 "$f" root@localhost:$B/ || { echo "copy $f failed"; echo "templates: 1 failed"; exit 1; }
done
scp -q $K -P 2222 "$G/my-indexer/atrium.toml" root@localhost:$B/indexer.toml
PSOCK=/atrium/sockets/portcullis/portcullis.sock
LOG=$B/portcullisd.log
V 'sh -s' <<EOS
set -e
install -m 0755 $B/portcullisd /usr/local/bin/portcullisd
install -m 0755 $B/portcullis /usr/local/bin/portcullis
service atrium-jaild stop >/dev/null 2>&1 || true
pkill -x atrium-jaild || true; sleep 1; rm -f /var/run/atrium/jaild.sock
install -m 0755 $B/atrium-jaild /usr/local/bin/atrium-jaild
mkdir -p /etc/atrium
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
[ $? = 0 ] && ok "portcullisd up, with the worker-call socket" || { bad "setup: $(V "tail -3 $LOG")"; echo "templates: $fails failed"; exit 1; }

A() { V "su -m alice -c 'cd /tmp; portcullis workspace $1'" 2>&1; }
W=/home/alice/.atrium/workspaces
A "stop scratch" >/dev/null
A "new scratch --network none" >/dev/null 2>&1
A "list" | grep -q scratch && ok "Scratch is there" || bad "no Scratch: $(A list)"
# Each project as the template made it, with the probe as its entry and no
# graphics (there is no frescod; the probe opens no window).
V 'sh -s' <<EOS
set -e
for p in viewer indexer; do rm -rf $W/scratch/work/my-\$p; mkdir -p $W/scratch/work/my-\$p/bin; done
P=$W/scratch/work/my-viewer
sed -e 's|^entry = "bin/my-viewer"|entry = "bin/probe"|' -e '/^graphics = /d' $B/atrium.toml > \$P/atrium.toml
install -m 0755 $B/probe \$P/bin/probe; install -m 0755 $B/my-viewer-decode \$P/bin/my-viewer-decode
P=$W/scratch/work/my-indexer
sed -e 's|^entry = "bin/my-indexer"|entry = "bin/probe"|' -e '/^graphics = /d' $B/indexer.toml > \$P/atrium.toml
install -m 0755 $B/probe \$P/bin/probe; install -m 0755 $B/my-indexer-index \$P/bin/my-indexer-index
chown -R alice:files-alice $W/scratch/work/my-viewer $W/scratch/work/my-indexer
EOS

line() { V "grep 'templates-gate $1 ' $LOG | tail -1" | sed "s/.*templates-gate $1 //"; }
waitfor() { i=0; while [ $i -lt 180 ] && ! V "grep -q 'templates-gate $1' $LOG"; do sleep 3; i=$((i+3)); done; }

echo "=== viewer"
out=$(A "run scratch my-viewer")
case "$out" in dev.alice.scratch.org.example.my-viewer*) ok "Run installs it" ;; *) bad "run: $out" ;; esac
waitfor "viewer done"
r=$(line "viewer picture"); case "$r" in "2x2 whole=true first=[48, 80, 112, 255]") ok "the seed decodes in the worker: $r" ;; *) bad "picture: $r ($(V "tail -5 $LOG"))" ;; esac
r=$(line "viewer small"); case "$r" in "1x1") ok "scaled to max_side 1" ;; *) bad "small: $r" ;; esac
r=$(line "viewer other"); case "$r" in "worker-error not a BMP picture") ok "a file that isn't a BMP is the worker's error" ;; *) bad "other: $r" ;; esac

echo "=== indexer"
out=$(A "run scratch my-indexer")
case "$out" in dev.alice.scratch.org.example.my-indexer*) ok "Run installs it" ;; *) bad "run: $out" ;; esac
waitfor "indexer done"
r=$(line "indexer counts"); case "$r" in "total=5 top=a:3 file=2") ok "one worker keeps the counts across calls: $r" ;; *) bad "indexer: $r ($(V "tail -5 $LOG"))" ;; esac

sleep 3
left=$(V "jls name | grep -c 'org.example.my-.*w-'" | tr -d ' ')
[ "$left" = 0 ] && ok "no worker jail left" || bad "$left worker jails left"
A "stop scratch" >/dev/null
echo "templates: $fails failed"
[ $fails = 0 ]

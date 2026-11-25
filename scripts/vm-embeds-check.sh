#!/bin/sh
# Dev-tools M6 (docs/spec/dev-tools-plan.md §8, app-composition.md §5.1):
# third-party embeds and providers from manifests. Runs on the HOST after
#   ARCA=amd64 sh scripts/cloud-vm.sh up      (booted on kernel.atrium)
#   sh scripts/vm-embeds-check.sh
# Ends "embeds: N failed". The filter denies (mode 2). No frescod, so an
# embed that gets past limend's checks ends at frescod's embed connection
# (refusal 9, LAUNCH_FAILED); what the gate checks is how far each gets.
#
# Three dev builds Run from Scratch (limen/examples):
#   - Share: `[roles] implements = ["editor-extension", "share-target"]`, no capabilities;
#   - Embedder: `embeds = ["doc-viewer", "editor-extension"]`, `arca = true`;
#   - Bare: the same program, no `embeds`.
# Embedder has limend's socket in its jail; Bare has none. limend lets
# Embedder ask for doc-viewer (no provider installed: refusal 2) and
# editor-extension only as a plugin it names (dev-tools.md §3.8): limend
# lists Share, gives it to Embedder, read from Share's manifest, and
# refuses Bare (not a plugin, 18):
# portcullisd starts Share as a uid of Embedder's instance (not limend's,
# not Embedder's); map and picker are refused (15, NOT_EMBEDDABLE).
# share-target has no default since the share sheet (share-sheet.md §3):
# without the person's choice in a sheet it is refused 2 (NO_PROVIDER).
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

echo "=== cross-build"
(cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" \
    -p portcullisd -p portcullis-cli --bin portcullisd --bin portcullis) \
    || { echo "build failed"; echo "embeds: 1 failed"; exit 1; }
(cd "$ROOT/limen" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" \
    --bin limend --example embeds_gate --example embeds_provider) \
    || { echo "limen build failed"; echo "embeds: 1 failed"; exit 1; }

echo "=== install"
B=/var/tmp/embeds-gate
V "pkill -x portcullisd; pkill -x limend; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B"
for f in "$ROOT/portcullis/target/$TARGET/release/portcullisd" "$ROOT/portcullis/target/$TARGET/release/portcullis" \
         "$ROOT/limen/target/$TARGET/release/limend" \
         "$ROOT/limen/target/$TARGET/release/examples/embeds_gate" "$ROOT/limen/target/$TARGET/release/examples/embeds_provider" \
         "$ROOT/etc/jaild.policy.toml"; do
    scp -q $K -P 2222 "$f" root@localhost:$B/ || { echo "copy $f failed"; echo "embeds: 1 failed"; exit 1; }
done
PSOCK=/atrium/sockets/portcullis/portcullis.sock
LOG=$B/portcullisd.log
LLOG=$B/limend.log
V 'sh -s' <<EOS
set -e
install -m 0755 $B/portcullisd /usr/local/bin/portcullisd
install -m 0755 $B/portcullis /usr/local/bin/portcullis
install -m 0755 $B/limend /usr/local/bin/limend
mkdir -p /etc/atrium
sed -e 's/^mode     = 1/mode     = 2/' $B/jaild.policy.toml > /etc/atrium/jaild.policy.toml
grep -q '^allow_rctl' /etc/atrium/jaild.policy.toml || printf '\n[resource_control]\nallow_rctl = true\n' >> /etc/atrium/jaild.policy.toml
id alice >/dev/null 2>&1 || pw useradd alice -m -s /bin/sh
id _limen >/dev/null 2>&1 || pw useradd _limen -u 65022 -d /nonexistent -s /usr/sbin/nologin
service atrium-jaild restart >/dev/null 2>&1 || service atrium-jaild start >/dev/null 2>&1 || true
mkdir -p /atrium/sockets/portcullis /atrium/sockets/limen /atrium/sockets/arca; rm -f $PSOCK /atrium/sockets/limen/limend.sock
chown _limen /atrium/sockets/limen
: > $LOG; : > $LLOG; chmod 666 $LLOG
daemon -f -o $LOG /usr/local/bin/portcullisd --socket $PSOCK
sleep 3; test -S $PSOCK; chmod 666 $PSOCK
daemon -f -u _limen -o $LLOG /usr/local/bin/limend
sleep 2; test -S /atrium/sockets/limen/limend.sock
EOS
[ $? = 0 ] && ok "portcullisd and limend (as _limen) up" || { bad "setup: $(V "tail -3 $LOG; tail -3 $LLOG")"; echo "embeds: $fails failed"; exit 1; }

A() { V "su -m alice -c 'cd /tmp; portcullis workspace $1'" 2>&1; }
W=/home/alice/.atrium/workspaces
A "stop scratch" >/dev/null
A "new scratch --network none" >/dev/null 2>&1
mk() { # folder id bin extra-toml
    V "rm -rf $W/scratch/work/$1; mkdir -p $W/scratch/work/$1/bin; install -m 0755 $B/$3 $W/scratch/work/$1/bin/app; \
       printf '[app]\nid = \"$2\"\nname = \"$1\"\nversion = \"1\"\nentry = \"bin/app\"\n$4' > $W/scratch/work/$1/atrium.toml; \
       chown -R alice:files-alice $W/scratch/work/$1"
}
mk share org.example.share embeds_provider '\n[roles]\nimplements = [\"editor-extension\", \"share-target\"]\n\n[roles.editor-extension]\nlanguages = [\"*\"]\n'
mk embedder org.example.embedder embeds_gate '\n[capabilities]\nembeds = [\"doc-viewer\", \"editor-extension\"]\narca = true\n'
mk bare org.example.bare embeds_gate ''

echo "=== Run"
out=$(A "run scratch share"); case "$out" in dev.alice.scratch.org.example.share*) ok "Share installed" ;; *) bad "share: $out" ;; esac
V "sleep 3; pkill -f 'dev.alice.scratch.org.example.share' >/dev/null 2>&1; true"
out=$(A "run scratch bare"); case "$out" in dev.alice.scratch.org.example.bare*) ok "Bare installed" ;; *) bad "bare: $out" ;; esac
out=$(A "run scratch embedder"); case "$out" in dev.alice.scratch.org.example.embedder*) ok "Embedder installed" ;; *) bad "embedder: $out" ;; esac
i=0; while [ $i -lt 90 ] && [ "$(V "grep -c 'embeds-gate done' $LOG")" -lt 1 ]; do sleep 3; i=$((i+3)); done
sleep 2
line() { V "grep 'embeds-gate $1 ' $LOG | tail -1" | sed "s/.*embeds-gate $1 //"; }
socks=$(V "grep 'embeds-gate socket' $LOG" | sed 's/.*socket //' | sort | tr '\n' ' ')
[ "$socks" = "no yes " ] && ok "limend's socket is in Embedder's jail and not in Bare's" || bad "sockets: [$socks]"
arcas=$(V "grep 'embeds-gate arca' $LOG" | sed 's/.*arca //' | sort | tr '\n' ' ')
[ "$arcas" = "no yes " ] && ok "arca = true puts arcad's socket in Embedder's jail, and Bare has none" || bad "arca: [$arcas]"
r=$(line doc-viewer); [ "$r" = "refused 2" ] && ok "doc-viewer passes the embeds check (no provider installed: 2)" || bad "doc-viewer: $r"
r=$(line editor-extension); [ "$r" = "refused 2" ] && ok "editor-extension as a plain embed: no provider (2), a plugin is named" || bad "editor-extension: $r ($(V "tail -5 $LLOG"))"
# (Other plugins installed on this guest, Artifex's samples, may be listed too.)
r=$(line plugins); case ",$r," in *",dev.alice.scratch.org.example.share:*,"*) ok "limend lists Share as Embedder's person's plugin" ;; *) bad "plugins: $r" ;; esac
# Starting the jail can outlast the client's 5 s under TCG: then limend's
# log says how far it got.
r=$(line plugin)
if [ "$r" = "refused 9" ] || { case "$r" in *"did not answer"*) true ;; *) false ;; esac && V "grep -q 'example.share for editor-extension: frescod refused' $LLOG"; }; then
    ok "Share named as a plugin goes to it (ends at frescod: $r)"
else bad "plugin: $r ($(V "tail -5 $LLOG"))"; fi
r=$(line not-a-plugin); [ "$r" = "refused 18" ] && ok "Bare named as a plugin is refused (18, NO_PLUGIN)" || bad "not-a-plugin: $r"
r=$(line share-target); [ "$r" = "refused 2" ] && ok "share-target without a choice in the share sheet: no provider (2)" || bad "share-target: $r"
r=$(line map); [ "$r" = "refused 15" ] && ok "map is refused: not in embeds" || bad "map: $r"
r=$(line picker); [ "$r" = "refused 15" ] && ok "picker is refused: not in embeds" || bad "picker: $r"
started=$(V "grep 'dev.alice.scratch.org.example.share for alice' $LOG | tail -1")
case "$started" in *"dev.alice.scratch.org.example.embedder runs as"*) ok "Share ran for Embedder's instance: ${started#portcullisd: }" ;; *) bad "share start: $started ($(V "grep -i share $LOG | tail -4"))" ;; esac
# The uid portcullisd ran it as (it is ended at once, before it can say so itself).
pu=$(echo "$started" | sed -n 's/.*runs as atrium-app-\([0-9]*\).*/\1/p')
eu=$(V "awk '\$3 == \"dev.alice.scratch.org.example.embedder\" {print \$1}' /var/run/atrium/app-registry | head -1")
if [ -n "$pu" ] && [ "$pu" != "$eu" ] && [ "$pu" != 65022 ]; then ok "as its own uid $pu (Embedder $eu, limend 65022)"; else bad "provider uid [$pu] Embedder [$eu]"; fi
r=$(V "grep 'may not embed' $LLOG | head -1"); case "$r" in *embedder*) ok "limend says why: ${r#limend: }" ;; *) bad "limend log: $r" ;; esac

A "stop scratch" >/dev/null
V "pkill -x limend; true"
echo "embeds: $fails failed"
[ $fails = 0 ]

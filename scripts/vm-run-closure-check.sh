#!/bin/sh
# Dev-tools M4 (docs/spec/dev-tools-plan.md §6): Run carries what a build
# links. Runs on the HOST after
#   ARCH=amd64 sh scripts/cloud-vm.sh up      (booted on kernel.atrium)
#   sh scripts/install-c-sdk.sh               (the C SDK in the base)
#   sh scripts/vm-run-closure-check.sh
# Ends "run-closure: N failed". The filter denies (mode 2). No frescod: the
# apps print a line to portcullisd's log instead of opening a window.
#
#   - in Scratch, with atrium.app.mk: a C program linking the SDK's
#     libfresco and jansson from pkg (pkgconf's pkg-config) builds, runs
#     there, and Runs as an app: its bundle has libfresco from the base at
#     /usr/lib and jansson from Scratch's /usr/local, and it prints;
#   - a library reached through a link out of work is refused, before
#     anything is installed;
#   - Scratch uses Liba: a program linking Liba's libgreet through
#     /workspaces/liba (pkg-config finds it by PKG_CONFIG_PATH) Runs, with
#     libgreet in its bundle;
#   - [bundle] extra-libs: a program that dlopens jansson (it doesn't link
#     it) Runs with jansson in its bundle and loads it; a pattern that
#     matches nothing is refused; make bundle (cargo-atrium, cross-built
#     here and copied in) writes a tree with it too.
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
CA=${CA:-/root/.ccr/ca-bundle.crt}

echo "=== cross-build"
(cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" \
    -p portcullisd -p portcullis-cli --bin portcullisd --bin portcullis) \
    || { echo "build failed"; echo "run-closure: 1 failed"; exit 1; }

echo "=== install"
B=/var/tmp/run-closure
V "pkill -x portcullisd; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B"
(cd "$ROOT/sdk" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" -p cargo-atrium) \
    || { echo "cargo-atrium build failed"; echo "run-closure: 1 failed"; exit 1; }
scp -q $K -P 2222 "$ROOT/sdk/target/$TARGET/release/cargo-atrium" root@localhost:/var/tmp/cargo-atrium-rc
for b in portcullisd portcullis; do
    scp -q $K -P 2222 "$ROOT/portcullis/target/$TARGET/release/$b" root@localhost:$B/ || { echo "copy $b failed"; echo "run-closure: 1 failed"; exit 1; }
done
scp -q $K -P 2222 "$ROOT/etc/jaild.policy.toml" root@localhost:$B/jaild.policy.toml
[ -f "$CA" ] && scp -q $K -P 2222 "$CA" root@localhost:$B/proxy-ca.pem
PSOCK=/atrium/sockets/portcullis/portcullis.sock
LOG=$B/portcullisd.log
V 'sh -s' <<EOS
set -e
install -m 0755 $B/portcullisd /usr/local/bin/portcullisd
install -m 0755 $B/portcullis /usr/local/bin/portcullis
mkdir -p /etc/atrium
sed -e 's/^mode     = 1/mode     = 2/' $B/jaild.policy.toml > /etc/atrium/jaild.policy.toml
grep -q '^allow_rctl' /etc/atrium/jaild.policy.toml || printf '\n[resource_control]\nallow_rctl = true\n' >> /etc/atrium/jaild.policy.toml
if [ -f $B/proxy-ca.pem ]; then
    [ -f /etc/ssl/cert.pem.pkg-bak ] || cp /etc/ssl/cert.pem /etc/ssl/cert.pem.pkg-bak
    cat /etc/ssl/cert.pem.pkg-bak $B/proxy-ca.pem > /etc/ssl/cert.pem
fi
test -f /usr/lib/libfresco.so.0 && test -f /usr/share/mk/atrium.app.mk
id alice >/dev/null 2>&1 || pw useradd alice -m -s /bin/sh
service atrium-jaild status >/dev/null 2>&1 || service atrium-jaild start >/dev/null 2>&1 || true
mkdir -p /atrium/sockets/portcullis; rm -f $PSOCK
: > $LOG
daemon -f -o $LOG /usr/local/bin/portcullisd --socket $PSOCK
sleep 3; test -S $PSOCK; chmod 666 $PSOCK
EOS
[ $? = 0 ] && ok "portcullisd up, the C SDK in the base" || { bad "setup (is the C SDK installed? $(V "tail -3 $LOG"))"; echo "run-closure: $fails failed"; exit 1; }

A() { V "su -m alice -c 'cd /tmp; portcullis workspace $1'" 2>&1; }
W=/home/alice/.atrium/workspaces
# A script run in a workspace, from stdin, as its uid, in ~/work (put
# there as alice's, run with the workspace's sh).
ws_sh() { V "cat > $W/$1/work/.step.sh && chown alice:files-alice $W/$1/work/.step.sh && chmod 664 $W/$1/work/.step.sh" && \
          V "su -m alice -c 'cd /tmp; portcullis workspace exec $1 -- /bin/sh .step.sh'" 2>&1; }
logline() { # marker -> its line in the log, waiting up to 30 s
    i=0; while [ $i -lt 30 ] && ! V "grep -q '$1' $LOG"; do sleep 2; i=$((i+2)); done
    V "grep '$1' $LOG | tail -1"
}
for w in scratch liba; do A "stop $w" >/dev/null; done
A "new liba --network none" >/dev/null
A "set scratch --network outbound --uses liba" >/dev/null
case "$(A list)" in *"scratch"*"uses=liba"*) ok "Scratch uses Liba" ;; *) bad "list: $(A list)" ;; esac

echo "=== jansson and pkgconf from pkg, in Scratch"
out=$(A "pkg scratch install -y jansson pkgconf")
case "$out" in *"pkg: exit 0"*) ok "pkg install jansson pkgconf" ;; *) bad "pkg: $(echo "$out" | tail -3)" ;; esac

echo "=== a C app with libfresco and jansson (atrium.app.mk)"
out=$(ws_sh scratch <<'SH'
set -e
mkdir -p ~/work/fr && cd ~/work/fr
cat > fr.c <<'C'
#include <stdio.h>
#include <stdint.h>
#include <fresco.h>
#include <jansson.h>
int main(void) {
    uint8_t blob[256];
    size_t n = fresco_blob_material_solid(blob, 1, 0, 0, 1);
    json_t *o = json_pack("{s:i}", "blob", (int)n);
    char *s = json_dumps(o, JSON_COMPACT);
    printf("closure-fresco %s\n", s);
    return 0;
}
C
printf 'PROG= fr\nATRIUM_LIBS= fresco jansson\n.include <atrium.app.mk>\n' > Makefile
make >/dev/null
make atrium.toml >/dev/null
./fr
SH
)
case "$out" in *'closure-fresco {"blob":'*) ok "builds with make and runs in Scratch: $(echo "$out" | grep closure-fresco)" ;; *) bad "build: $out" ;; esac
out=$(A "run scratch fr")
case "$out" in dev.alice.scratch.org.example.fr*) ok "Run installs it" ;; *) bad "run: $out" ;; esac
APP=/var/lib/atrium/apps/dev.alice.scratch.org.example.fr
check "its bundle has the SDK's libfresco at /usr/lib" "1" "$(V "test -f $APP/usr/lib/libfresco.so.0 && echo 1")"
check "and Scratch's jansson at /usr/lib" "1" "$(V "test -f $APP/usr/lib/libjansson.so.4 && echo 1")"
check "and the loader" "1" "$(V "test -f $APP/libexec/ld-elf.so.1 && echo 1")"
L=$(logline 'closure-fresco')
case "$L" in *'{"blob":'*) ok "it ran as an app: $L" ;; *) bad "no line from the app: $(V "tail -5 $LOG")" ;; esac

echo "=== a library through a link out of work"
out=$(ws_sh scratch <<'SH'
set -e
mkdir -p /tmp/outside ~/work/esc/lib && cd ~/work/esc
printf 'int esc(void){return 7;}\n' > /tmp/outside/e.c
cc -shared -fPIC -Wl,-soname,libesc.so.1 -o /tmp/outside/libesc.so.1 /tmp/outside/e.c
ln -sf /tmp/outside/libesc.so.1 lib/libesc.so.1
ln -sf libesc.so.1 lib/libesc.so
printf '#include <stdio.h>\nint esc(void);\nint main(void){printf("closure-esc %%d\\n", esc());return 0;}\n' > esc.c
cc -o esc esc.c -Llib -lesc '-Wl,-rpath,$ORIGIN/lib'
printf '[app]\nid = "org.example.esc"\nname = "Esc"\nversion = "1"\nentry = "esc"\n' > atrium.toml
./esc
SH
)
case "$out" in *"closure-esc 7"*) ok "it runs in Scratch" ;; *) bad "esc build: $out" ;; esac
out=$(A "run scratch esc")
case "$out" in *"link out of"*) ok "Run refuses it: $(echo "$out" | cut -c1-110)" ;; *) bad "esc run: $out" ;; esac
check "and nothing is installed" "" "$(V "ls -d /var/lib/atrium/apps/dev.alice.scratch.org.example.esc 2>/dev/null")"

echo "=== a library from Liba, through Scratch's uses link"
out=$(ws_sh liba <<'SH'
set -e
P=~/work/.atrium/prefix
mkdir -p ~/work/greet $P/lib/pkgconfig $P/include && cd ~/work/greet
printf 'const char *greet(void){return "hello from liba";}\n' > greet.c
printf 'const char *greet(void);\n' > $P/include/greet.h
cc -shared -fPIC -Wl,-soname,libgreet.so.1 -o $P/lib/libgreet.so.1 greet.c
ln -sf libgreet.so.1 $P/lib/libgreet.so
printf 'prefix=/workspaces/liba/.atrium/prefix\nName: greet\nDescription: g\nVersion: 1\nLibs: -L${prefix}/lib -Wl,-rpath,${prefix}/lib -lgreet\nCflags: -I${prefix}/include\n' > $P/lib/pkgconfig/greet.pc
echo built
SH
)
case "$out" in *built*) ok "Liba builds libgreet" ;; *) bad "liba: $out" ;; esac
out=$(ws_sh scratch <<'SH'
set -e
mkdir -p ~/work/hi && cd ~/work/hi
printf '#include <stdio.h>\n#include <greet.h>\nint main(void){printf("closure-uses %%s\\n", greet());return 0;}\n' > hi.c
printf 'PROG= hi\nATRIUM_LIBS= greet\n.include <atrium.app.mk>\n' > Makefile
echo "PKG_CONFIG_PATH=$PKG_CONFIG_PATH"
make >/dev/null && make atrium.toml >/dev/null && ./hi
SH
)
case "$out" in *"closure-uses hello from liba"*) ok "Scratch builds and runs it ($(echo "$out" | grep PKG_CONFIG_PATH))" ;; *) bad "hi build: $out" ;; esac
out=$(A "run scratch hi")
case "$out" in dev.alice.scratch.org.example.hi*) ok "Run installs it" ;; *) bad "hi run: $out" ;; esac
check "its bundle has libgreet" "1" "$(V "test -f /var/lib/atrium/apps/dev.alice.scratch.org.example.hi/usr/lib/libgreet.so.1 && echo 1")"
L=$(logline 'closure-uses')
case "$L" in *"hello from liba"*) ok "it ran as an app: $L" ;; *) bad "no line from hi: $(V "tail -5 $LOG")" ;; esac

echo "=== [bundle] extra-libs: a library the app dlopens"
V "mkdir -p $W/scratch/work/.bin && install -o alice -g files-alice -m 0755 /var/tmp/cargo-atrium-rc $W/scratch/work/.bin/cargo-atrium"
out=$(ws_sh scratch <<'SH'
set -e
mkdir -p ~/work/dl && cd ~/work/dl
cat > dl.c <<'C'
#include <stdio.h>
#include <dlfcn.h>
int main(void) {
    void *h = dlopen("libjansson.so.4", RTLD_NOW);
    printf("closure-dl %s\n", h && dlsym(h, "json_object") ? "loaded" : dlerror());
    return 0;
}
C
printf 'PROG= dl\nLDFLAGS+= -Wl,-rpath,/usr/local/lib\n.include <atrium.app.mk>\n' > Makefile
make >/dev/null
printf '[app]\nid = "org.example.dl"\nname = "Dl"\nversion = "1"\nentry = "dl"\n\n[bundle]\nextra-libs = ["libnothere*.so*"]\n' > atrium.toml
./dl
SH
)
case "$out" in *"closure-dl loaded"*) ok "dl builds and loads jansson in Scratch" ;; *) bad "dl build: $out" ;; esac
out=$(A "run scratch dl")
case "$out" in *"libnothere"*) ok "Run refuses a pattern that matches nothing: $(echo "$out" | cut -c1-100)" ;; *) bad "dl nothere: $out" ;; esac
ws_sh scratch >/dev/null <<'SH'
cd ~/work/dl && sed -i '' 's/libnothere\*\.so\*/libjansson.so.*/' atrium.toml
SH
out=$(A "run scratch dl")
case "$out" in dev.alice.scratch.org.example.dl*) ok "Run installs it with extra-libs" ;; *) bad "dl run: $out" ;; esac
check "its bundle has jansson, which nothing links" "1" "$(V "test -f /var/lib/atrium/apps/dev.alice.scratch.org.example.dl/usr/lib/libjansson.so.4 && echo 1")"
L=$(logline 'closure-dl')
case "$L" in *loaded*) ok "it dlopens it as an app: $L" ;; *) bad "dl as an app: $L $(V "tail -5 $LOG")" ;; esac
out=$(ws_sh scratch <<'SH'
cd ~/work/dl && make bundle CARGO_ATRIUM=$HOME/work/.bin/cargo-atrium 2>&1 | tail -4
find bundle -name 'libjansson*' | head -3
SH
)
case "$out" in *"lib/libjansson.so.4"*) ok "make bundle carries it: $(echo "$out" | grep libjansson | head -1)" ;; *) bad "make bundle: $out" ;; esac
out=$(ws_sh scratch <<'SH'
cd ~/work/dl && make bundle CARGO_ATRIUM=no-such-tool 2>&1 | head -2
SH
)
case "$out" in *"needs cargo-atrium"*) ok "without cargo-atrium make bundle says what to add" ;; *) bad "no cargo-atrium: $out" ;; esac

for w in scratch liba; do A "stop $w" >/dev/null; done
V "[ -f /etc/ssl/cert.pem.pkg-bak ] && cp /etc/ssl/cert.pem.pkg-bak /etc/ssl/cert.pem; true"
echo "run-closure: $fails failed"
[ $fails = 0 ]

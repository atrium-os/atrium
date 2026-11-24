#!/bin/sh
# Curia's Network page live (docs/spec/curia.md §5 C1) in the FreeBSD VM,
# against real Wi-Fi: atrium-net and aura-mlme/aura-keys as the Aura gate
# leaves them, atrium-netconf, headless frescod + forum-wm + forum-bar, and
# Curia as `appui`. Frames go to scratch/curia-*.png.
#
#   sh scripts/aura-sim-gate.sh vms && sh scripts/aura-sim-gate.sh bar   # Wi-Fi and the UI up
#   sh scripts/curia-vm.sh arca                 # Arca's field (C2): user alice's store
#                                               # unlocked, frescod again with slots, limend,
#                                               # arca-ui; then Curia, one frame
#   sh scripts/curia-vm.sh shot                 # start Curia, one frame
#   sh scripts/curia-vm.sh click X Y [name]     # a click, then a frame
#   sh scripts/curia-vm.sh type TEXT [name]     # keys (a-z, 0-9, @ . / , - space, Enter as |, Backspace as <), then a frame
#   sh scripts/curia-vm.sh frame [name]
#   sh scripts/curia-vm.sh status               # atrium-netctl status + list
#   sh scripts/curia-vm.sh away                 # after `arca`: each service Settings talks to
#                                               # dies or hangs and comes back; frames curia-away-*.png
#   sh scripts/curia-vm.sh display              # forum-wm with its settings socket, then
#                                               # Curia's Display page (C4a), one frame
#   sh scripts/curia-vm.sh sound                # after `cloud-vm.sh lyra` (VM booted with
#                                               # lyra-vm-check.sh qemu-args): lyrad, choragusd
#                                               # with the control socket, frescod, forum-wm,
#                                               # a tone as org.test.a, Curia's Sound page
#   sh scripts/curia-vm.sh picker               # after `arca`: Scrinium as the system picker,
#                                               # its one place /tmp/aura/certs holding the
#                                               # corp CA that aura-sim-gate.sh bar-eap kept
#   sh scripts/curia-vm.sh sign                 # the corp CA signs the newest certificate
#                                               # request aura-keys answered (EAP-TLS, as IT
#                                               # would) into /tmp/aura/certs/tls-user.pem
#   sh scripts/curia-vm.sh core [TEXT]          # Curia's core: TEXT (default the gate's
#                                               # passphrase) must not be in it
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
T=aarch64-unknown-freebsd
S="$HOME/atrium-vm/sysroot"
V="$ROOT/scripts/vssh"
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
Sx=/tmp/st
D=/opt/bar-net
O="$ROOT/scratch"; mkdir -p "$O"
X="-C link-arg=--sysroot=$S -C link-arg=-L$S/lib -C link-arg=-L$S/usr/lib -C link-arg=--dynamic-linker=/libexec/ld-elf.so.1 -C link-arg=$S/usr/lib/Scrt1.o -C link-arg=$S/usr/lib/crti.o -C link-arg=$S/usr/lib/crtbeginS.o -C link-arg=-lc -C link-arg=$S/usr/lib/crtendS.o -C link-arg=$S/usr/lib/crtn.o"
inj() { "$V" "printf '$1\n' | nc -N -U $Sx/inject.sock"; }
frame() {
    sleep "${GAP:-20}"
    "$V" "cp $Sx/screen.png $Sx/f.png"
    scp $K -P 2222 -q root@localhost:$Sx/f.png "$O/curia-$1.png"
    echo "frame: $O/curia-$1.png"
}
PUB=$(printf 'c%.0s' $(seq 64))
AS=/atrium/sockets/arca/arcad.sock
E2="FRESCO_SOCKET=$Sx/fresco.sock FRESCO_WM_SOCKET=$Sx/wm.sock FORUM_CTL_SOCKET=$Sx/ctl.sock DISPLAY_SETTINGS_SOCKET=$Sx/display.sock"
case "${1:-}" in
arca)
    for c in "limen --bin limend" "arca/arca-ui --bin arca-ui" "arca/arcad --bin arca-probe"; do
        set -- $c
        (cd "$ROOT/$1" && RUSTFLAGS="$X" cargo build -q --release --target "$T" $2 $3) || exit 1
    done
    # A running binary can't be replaced over scp.
    "$V" "pkill -x limend; pkill -x arca-ui; pkill -x curia; sleep 1; true"
    for b in limen/target/$T/release/limend arca/arca-ui/target/$T/release/arca-ui arca/arcad/target/$T/release/arca-probe; do
        scp $K -P 2222 -q "$ROOT/$b" "root@localhost:$D/"
    done
    # Curia (appui, 50004) is alice's Settings app in the launch registry
    # arcad, limend and arca-ui read; Vestibulum's stand-in unlocks her store.
    "$V" "set -e
        id sg-unlock >/dev/null 2>&1 || pw useradd sg-unlock -u 50093 -d /nonexistent -s /usr/sbin/nologin
        id sg-ui >/dev/null 2>&1 || pw useradd sg-ui -u 65021 -d /nonexistent -s /usr/sbin/nologin
        sed -i '' '/^50004 /d' /tmp/aura/launch-registry
        printf '50004 alice org.atrium.curia $PUB\n' >> /tmp/aura/launch-registry
        chmod 644 /tmp/aura/launch-registry
        mkdir -p /atrium/sockets/ostiarius; printf 'alice\n' > /atrium/sockets/ostiarius/active-session; : > /atrium/sockets/ostiarius/session-locked
        r=\$(su -m sg-unlock -c '$D/arca-probe $AS unlock alice correct-horse'); echo \"arca: \$r\"
        for p in curia forum-bar forum-wm arca-ui limend frescod; do pkill -x \$p || true; done; sleep 2; rm -f $Sx/*.sock
        cd $Sx && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=$Sx/screen FRESCOD_SOCK=$Sx/fresco.sock FRESCOD_CHROME_UIDS=50004 FRESCOD_WM_SOCK=$Sx/wm.sock \
            FRESCOD_INPUT_SOCK=$Sx/inject.sock FRESCOD_ACTIVATION_SOCK=$Sx/activation.sock FRESCOD_SLOT_UIDS=0 FRESCOD_ACTIVATION_UIDS=65021 FRESCOD_INJECT_ACTIVATION=1 \
            daemon -f -o $Sx/frescod.log $D/frescod </dev/null
        i=0; while [ \$i -lt 60 ] && ! test -S $Sx/fresco.sock; do sleep 2; i=\$((i+1)); done
        env $E2 daemon -f -o $Sx/wm.log $D/forum-wm </dev/null; sleep 4; chmod 666 $Sx/*.sock
        env $E2 FORUM_BAR_NET_DIR=/tmp/aura/sockets daemon -f -u appui -o $Sx/bar.log $D/forum-bar </dev/null
        env LIMEN_SOCKET=$Sx/limend.sock LIMEN_FRESCOD_ACTIVATION=$Sx/activation.sock LIMEN_APP_REGISTRY=/tmp/aura/launch-registry LIMEN_ARCA_UID=65021 \
            daemon -f -o $Sx/limend.log $D/limend </dev/null
        i=0; while [ \$i -lt 20 ] && ! test -S $Sx/limend.sock; do sleep 1; i=\$((i+1)); done; chmod 666 $Sx/limend.sock
        env LIMEN_SOCKET=$Sx/limend.sock FRESCO_SOCKET=$Sx/fresco.sock ARCA_SOCKET=$AS ARCA_APP_REGISTRY=/tmp/aura/launch-registry \
            daemon -f -o $Sx/arca-ui.log -u sg-ui $D/arca-ui </dev/null
        sleep 3; pgrep -lx 'frescod|forum-wm|forum-bar|limend|arca-ui' | tr '\n' ' '; echo"
    exec sh "$0" shot ;;
away)
    # Each service the pages use goes away (killed, or stopped so it hangs
    # with its socket open) and comes back; the page must say so and recover
    # by itself, with no restart of Settings. Frames show each step.
    NC="daemon -f -o /tmp/aura/netconf.log /tmp/aura/atrium-netconf -v --ctl /tmp/aura/net.sock --dir /tmp/aura/sockets --uid 65036 --capsicum"
    pid0=$("$V" "pgrep -x curia")
    "$V" "pkill -x atrium-netconf; sleep 1; rm -rf /tmp/aura/sockets"
    GAP=8 frame away-1-netconf-killed
    "$V" "$NC"
    GAP=12 frame away-2-netconf-back
    "$V" "pkill -STOP -x atrium-netconf"
    GAP=50 frame away-3-netconf-hung
    "$V" "pkill -CONT -x atrium-netconf"
    GAP=40 frame away-4-netconf-answers
    inj "MOVE 70 263"; sleep 2; inj "BTN 1 1"; sleep 1; inj "BTN 1 0"
    "$V" "service atrium-dispensatord stop >/dev/null"
    GAP=10 frame away-5-devices-gone
    "$V" "service atrium-dispensatord start >/dev/null"
    GAP=12 frame away-6-devices-back
    inj "MOVE 70 111"; sleep 2; inj "BTN 1 1"; sleep 1; inj "BTN 1 0"
    # Arca's field: a network that is not saved, its row open.
    for i in 1 2; do inj "MOVE 1216 332"; sleep 2; inj "BTN 1 1"; sleep 1; inj "BTN 1 0"; sleep 6; done
    sleep 10; inj "MOVE 300 242"; sleep 2; inj "BTN 1 1"; sleep 1; inj "BTN 1 0"
    GAP=12 frame away-7-field
    "$V" "pkill -x arca-ui"
    GAP=8 frame away-8-arca-ui-killed
    "$V" "cd $Sx && env LIMEN_SOCKET=$Sx/limend.sock FRESCO_SOCKET=$Sx/fresco.sock ARCA_SOCKET=$AS ARCA_APP_REGISTRY=/tmp/aura/launch-registry daemon -f -o $Sx/arca-ui.log -u sg-ui $D/arca-ui </dev/null"
    GAP=12 frame away-9-arca-ui-back
    "$V" "pkill -x limend; pkill -x arca-ui; true"
    GAP=8 frame away-10-limend-killed
    "$V" "cd $Sx && env LIMEN_SOCKET=$Sx/limend.sock LIMEN_FRESCOD_ACTIVATION=$Sx/activation.sock LIMEN_APP_REGISTRY=/tmp/aura/launch-registry LIMEN_ARCA_UID=65021 daemon -f -o $Sx/limend.log $D/limend </dev/null; sleep 3; chmod 666 $Sx/limend.sock
        env LIMEN_SOCKET=$Sx/limend.sock FRESCO_SOCKET=$Sx/fresco.sock ARCA_SOCKET=$AS ARCA_APP_REGISTRY=/tmp/aura/launch-registry daemon -f -o $Sx/arca-ui.log -u sg-ui $D/arca-ui </dev/null"
    GAP=15 frame away-11-limend-back
    pid1=$("$V" "pgrep -x curia")
    [ "$pid0" = "$pid1" ] && echo "ok   Settings ran throughout (pid $pid0)" || echo "FAIL Settings restarted: $pid0 -> $pid1"
    "$V" "grep -E 'limend|frescod' $Sx/curia.log | tail -5" ;;
display)
    (cd "$ROOT/forum-wm" && RUSTFLAGS="$X" cargo build -q --release --target "$T" --bin forum-wm) || exit 1
    "$V" "pkill -x forum-wm; sleep 2; true"
    scp $K -P 2222 -q "$ROOT/forum-wm/target/$T/release/forum-wm" "root@localhost:$D/"
    "$V" "cd $Sx && env $E2 daemon -f -o $Sx/wm.log $D/forum-wm </dev/null; sleep 4; chmod 666 $Sx/*.sock; ls -l $Sx/display.sock"
    GAP=${GAP:-40} sh "$0" shot
    inj "MOVE 70 187"; sleep 2; inj "BTN 1 1"; sleep 1; inj "BTN 1 0"
    GAP=8 frame display ;;
sound)
    for c in frescod forum-wm curia; do
        (cd "$ROOT/$c" && RUSTFLAGS="$X" cargo build -q --release --target "$T" --bin $c) || exit 1
    done
    L=/var/run/lyra-test; AC=/atrium/sockets/audio-control/audio-control.sock
    "$V" "for p in curia forum-bar forum-wm frescod lyra-play; do pkill -x \$p; done; pkill -f 'daemon.*lyra-test'; pkill lyrad; pkill choragusd; sleep 2
        rm -rf $Sx; mkdir -p $D $Sx /usr/local/share/fonts/ibm-plex; chmod 777 $Sx; true"
    for c in frescod forum-wm curia; do scp $K -P 2222 -q "$ROOT/$c/target/$T/release/$c" "root@localhost:$D/"; done
    for f in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
        "$V" "test -f /usr/local/share/fonts/ibm-plex/$(basename "$f")" || scp $K -P 2222 -q "$f" root@localhost:/usr/local/share/fonts/ibm-plex/
    done
    "$V" "set -e; kldload -n snd_hda; install -d -o _choragus -g _choragus -m 0755 /atrium/sockets/audio-control
        daemon -r -R 1 -u _lyra -o $L/lyrad.log -t lyra-test-lyrad /usr/local/bin/lyrad --serve </dev/null >/dev/null 2>&1
        daemon -r -R 1 -u _choragus -o $L/choragusd.log -t lyra-test-choragusd /usr/local/bin/choragusd --daemon $L/audio.sock /atrium/sockets/lyrad/lyrad.sock \
            --grants $L/grants --app-registry $L/registry --control $AC --device $L/device.sock --state /var/db/choragus </dev/null >/dev/null 2>&1
        for i in \$(seq 1 30); do [ -S $AC ] && break; sleep 1; done; ls -l $AC
        cd $Sx && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=$Sx/screen FRESCOD_SOCK=$Sx/fresco.sock FRESCOD_CHROME_UIDS=50004 FRESCOD_WM_SOCK=$Sx/wm.sock \
            FRESCOD_INPUT_SOCK=$Sx/inject.sock daemon -f -o $Sx/frescod.log $D/frescod </dev/null
        i=0; while [ \$i -lt 60 ] && ! test -S $Sx/fresco.sock; do sleep 2; i=\$((i+1)); done
        env $E2 daemon -f -o $Sx/wm.log $D/forum-wm </dev/null; sleep 4; chmod 666 $Sx/*.sock
        daemon -f -u appa -o $Sx/play.log /usr/local/bin/lyra-play $L/audio.sock media 900 </dev/null
        env $E2 CURIA_AUDIO_SOCK=$AC CURIA_NET_DIR=/nonexistent CURIA_DEVICES_SOCK=/nonexistent daemon -f -u appui -o $Sx/curia.log $D/curia </dev/null"
    sleep 60
    inj "MOVE 70 225"; sleep 2; inj "BTN 1 1"; sleep 1; inj "BTN 1 0"
    GAP=10 frame sound ;;
core)
    t=${2:-correct horse battery staple}
    "$V" "rm -f $Sx/curia.core; gcore -c $Sx/curia.core \$(pgrep -x curia) >/dev/null 2>&1; ls -l $Sx/curia.core | awk '{print \"core:\", \$5, \"bytes\"}'
        n=\$(grep -c -a -F '$t' $Sx/curia.core); echo \"passphrase found in Curia's core: \$n\"
        h=\$(printf '%s' '$t' | od -An -tx1 | tr -d ' \n'); m=\$(grep -c -a -F \"\$h\" $Sx/curia.core); echo \"as hex: \$m\"; rm -f $Sx/curia.core" ;;
shot)
    (cd "$ROOT/curia" && RUSTFLAGS="$X" cargo build -q --release --target "$T" --bin curia)
    "$V" "pkill -x curia; true"
    scp $K -P 2222 -q "$ROOT/curia/target/$T/release/curia" "root@localhost:$D/"
    "$V" "cd $Sx && env $E2 LIMEN_SOCKET=$Sx/limend.sock CURIA_NET_DIR=/tmp/aura/sockets daemon -f -u appui -o $Sx/curia.log $D/curia </dev/null"
    GAP=${GAP:-60} frame start ;;
click)
    inj "MOVE $2 $3"; sleep 2; inj "BTN 1 1"; sleep 1; inj "BTN 1 0"
    frame "${4:-click}" ;;
type)
    s=$2; out=""
    while [ -n "$s" ]; do
        c=$(printf '%s' "$s" | cut -c1); s=$(printf '%s' "$s" | cut -c2-)
        case "$c" in
            [a-z]) u=$(( $(printf '%d' "'$c") - 97 + 4 )) ;;
            [1-9]) u=$(( c + 29 )) ;;
            0) u=39 ;;
            .) u=55 ;;
            /) u=56 ;;
            ,) u=54 ;;
            -) u=45 ;;
            " ") u=44 ;;
            "|") u=40 ;;   # Enter
            @) out="${out}KEY 31 1 2\nKEY 31 0 2\n"; continue ;;   # Shift-2 (modifier byte 2: left Shift)
            "<") u=42 ;;   # Backspace
            *) echo "type: only a-z, 0-9, @ . / , -, space, | (Enter) and < (Backspace)"; exit 2 ;;
        esac
        out="${out}KEY $u 1\nKEY $u 0\n"
    done
    "$V" "printf '$out' | nc -N -U $Sx/inject.sock"
    frame "${3:-typed}" ;;
frame)
    GAP=${GAP:-2} frame "${2:-now}" ;;
picker)
    W="$ROOT/scratch/w7-ca"
    test -f "$W/corp-ca.pem" || { echo "no $W/corp-ca.pem: run aura-sim-gate.sh bar-eap"; exit 1; }
    (cd "$ROOT/scrinium" && RUSTFLAGS="$X" cargo build -q --release --target "$T" --bin scrinium) || exit 1
    "$V" "pkill -x scrinium; sleep 1; true"
    scp $K -P 2222 -q "$ROOT/scrinium/target/$T/release/scrinium" "root@localhost:$D/"
    "$V" 'mkdir -p /tmp/aura/certs && cat > /tmp/aura/certs/corp-ca.pem' < "$W/corp-ca.pem"
    # Scrinium (sg-files) is alice's picker in the registry; limend lets
    # Curia embed it from Curia's installed manifest.
    "$V" "set -e
        id sg-files >/dev/null 2>&1 || pw useradd sg-files -u 50097 -d /nonexistent -s /usr/sbin/nologin
        sed -i '' '/^50097 /d' /tmp/aura/launch-registry
        printf '50097 alice org.atrium.scrinium $PUB\n' >> /tmp/aura/launch-registry
        mkdir -p /var/lib/atrium/apps/org.atrium.curia
        chmod 755 /tmp/aura/certs; chmod 644 /tmp/aura/certs/*"
    "$V" 'cat > /var/lib/atrium/apps/org.atrium.curia/atrium.toml' < "$ROOT/curia/atrium.toml"
    "$V" "cd $Sx && env HOME=/nonexistent SCRINIUM_HOME=/nonexistent SCRINIUM_WORKSPACES=/nonexistent LIMEN_SOCKET=$Sx/limend.sock FRESCO_SOCKET=$Sx/fresco.sock \
            SCRINIUM_PLACES='Certificates=/tmp/aura/certs' daemon -f -u sg-files -o $Sx/picker.log $D/scrinium --no-window </dev/null
        i=0; until grep -q 'providing picker' $Sx/picker.log; do i=\$((i+1)); [ \$i -lt 30 ] || { tail -5 $Sx/picker.log; exit 1; }; sleep 1; done
        echo 'picker: Scrinium provides it'" ;;
sign)
    W="$ROOT/scratch/w7-ca"
    csr=$("$V" 'cat /var/log/atrium/atrium-aura-keys.log /tmp/aura/keys.log 2>/dev/null' | sed -n 's/.*-> key id=[0-9a-f]* csr=\([0-9a-f]*\).*/\1/p' | tail -1)
    [ -n "$csr" ] || { echo "no certificate request in aura-keys' log"; exit 1; }
    python3 -c "import sys; sys.stdout.buffer.write(bytes.fromhex(sys.argv[1]))" "$csr" > "$W/tls.csr.der"
    printf 'extendedKeyUsage=clientAuth\nbasicConstraints=CA:FALSE\n' > "$W/ext"
    openssl req -in "$W/tls.csr.der" -inform DER -verify -noout -subject || exit 1
    openssl x509 -req -in "$W/tls.csr.der" -inform DER -CA "$W/corp-ca.pem" -CAkey "$W/corp-ca.key" -CAcreateserial -days 30 -out "$W/tls-user.pem" -extfile "$W/ext" || exit 1
    "$V" 'cat > /tmp/aura/certs/tls-user.pem && chmod 644 /tmp/aura/certs/tls-user.pem' < "$W/tls-user.pem"
    echo "signed: /tmp/aura/certs/tls-user.pem" ;;
status)
    "$V" "/tmp/aura/atrium-netctl --sock /tmp/aura/net.sock status; /tmp/aura/atrium-netctl --sock /tmp/aura/net.sock list; tail -n 5 $Sx/curia.log" ;;
*)
    sed -n 2,31p "$0"; exit 2 ;;
esac

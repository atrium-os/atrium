#!/bin/sh
# Deploy the Portcullis stack (jaild + atrium-volumes + portcullisd) to the
# running dev VM.
#
# WHY THIS EXISTS
#
#   bootstrap-atrium.sh's `userspace` phase builds only portcullisd,
#   portcullis-cli and opifex from the portcullis workspace — NOT jaild,
#   atrium-volumes, or the portcullisd daemon/bootstrap binaries. And nothing
#   installs the rc.d scripts or /etc/atrium config at all. So a freshly built
#   devroot has the app trees and the CLI but no running stack, and
#   `service atrium-jaild status` reports "does not exist in /etc/rc.d",
#   which reads like a broken install rather than a missing step.
#
#   That is how the Tessera devroot ended up with /var/lib/atrium/apps
#   populated and no way to launch anything from it.
#
# ORDER MATTERS. The rc.d REQUIRE lines encode it:
#   atrium-jaild -> atrium-volumes -> atrium-portcullisd-daemon
#                                  -> atrium-portcullisd-bootstrap
#   atrium-jaild -> atrium-dispensatord (devices; docs/spec/device-assignment.md)
#
#   sh scripts/deploy-portcullis.sh              # deploy + enable + start
#   START=0 sh scripts/deploy-portcullis.sh      # install only
#   EGRESS_IF=vtnet0 sh scripts/deploy-portcullis.sh   # NAT interface, when
#                     the guest has no default route and several interfaces
set -u
EGRESS_IF="${EGRESS_IF:-}"
case "$EGRESS_IF" in *[!a-z0-9]*) echo "EGRESS_IF must be an interface name" >&2; exit 2;; esac
BSD="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="${TARGET:-aarch64-unknown-freebsd}"
T="$BSD/portcullis/target/$TARGET/release"
DT="$BSD/dispensator/target/$TARGET/release"
KEY="$HOME/.ssh/fresco_bsd_ed25519"
START=${START:-1}
SSHOPT="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
g() { ssh $SSHOPT -p 2222 root@localhost "$@"; }

# ── build anything missing ──────────────────────────────────────────
if [ ! -x "$DT/atrium-dispensatord" ]; then
    echo "=== building atrium-dispensatord (cross) ==="
    # The sysroot link flags cloud-vm.sh computes (a plain cargo build has no
    # FreeBSD libc to link against); CROSS_RUSTFLAGS in the environment wins.
    case "$TARGET" in x86_64-*) VMARCH=amd64 ;; *) VMARCH=aarch64 ;; esac
    : "${CROSS_RUSTFLAGS:=$(cd "$BSD" && ARCH=$VMARCH sh -c '. ./scripts/cloud-vm.sh >/dev/null 2>&1; printf %s "$CROSS_RUSTFLAGS"' sh env)}"
    ( cd "$BSD/dispensator" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build --release --target "$TARGET" ) || {
        echo "ABORT: dispensatord cross-build failed"; exit 1; }
fi
# ★ Cross-build on the HOST, never in the VM (feedback_always_cross_build).
# portcullis and opifex too: a guest base's older opifex refuses manifests
# with roles it doesn't know (e.g. [roles.app-settings]), so skipping them
# leaves the guest unable to install this tree's bundles.
if [ ! -x "$T/atrium-jaild" ] || [ ! -x "$T/atrium-volumes" ] || [ ! -x "$T/portcullis" ] || [ ! -x "$T/opifex" ]; then
    echo "=== building missing daemons and tools (cross) ==="
    ( cd "$BSD/portcullis" && TESSERA_CORE_LIB="$BSD/atrium-tessera/core" \
        cargo build --release --target "$TARGET" \
        -p jaild -p atrium-volumes -p portcullisd -p portcullis-cli -p opifex ) || {
        echo "ABORT: cross-build failed"; exit 1; }
fi

# ★ Stop the daemons BEFORE copying. FreeBSD refuses to write a running
# executable (ETXTBSY), so a deploy onto a live stack copied nothing for
# exactly the three binaries that matter, then reported "already running" and
# exited 0 — a green deploy that deployed the OLD code. Caught by comparing
# sha256 at the destination (feedback_verify_the_input_not_just_the_output).
# They are restarted below in dependency order; with START=0 the operator
# restarts them.
echo "=== stopping daemons (ETXTBSY guard) ==="
# The bootstrap too: started at boot, its binary is busy like the others'.
# Running is asked of rc.d (`service status`), never `pgrep -f`: the
# bootstrap's own arguments name atrium-volumes' socket, so a pattern match
# finds a daemon that isn't there.
running() { g "service $1 status >/dev/null 2>&1"; }
BOOTSTRAP_WAS=0
running atrium-portcullisd-bootstrap && BOOTSTRAP_WAS=1
for s in atrium-portcullisd-bootstrap atrium-dispensatord atrium-portcullisd-daemon atrium-volumes atrium-jaild; do
    if running $s; then
        g "service $s stop >/dev/null 2>&1"
        printf '  %-34s stopped\n' "$s"
    else
        printf '  %-34s not running\n' "$s"
    fi
done

echo "=== binaries -> /usr/local/bin ==="
rc=0
for b in atrium-jaild atrium-volumes atrium-volumes-cli \
         atrium-portcullisd-daemon atrium-portcullisd-bootstrap \
         atrium-portcullisd-jclient atrium-portcullisd-aq \
         portcullisd portcullis opifex ostiarius; do
    if [ -x "$T/$b" ]; then
        if scp $SSHOPT -P 2222 "$T/$b" root@localhost:/usr/local/bin/ >/dev/null 2>&1; then
            # Gate on the INPUT landing, not on scp's exit status: assert the
            # bytes AT THE DESTINATION are the bytes we built.
            want=$(shasum -a 256 "$T/$b" | awk '{print $1}')
            got=$(g "sha256 -q /usr/local/bin/$b" 2>/dev/null | tr -d '\r')
            if [ "$want" = "$got" ]; then
                printf '  %-34s ok\n' "$b"
            else
                printf '  %-34s SHA MISMATCH (want %.12s got %.12s)\n' "$b" "$want" "$got"
                rc=1
            fi
        else printf '  %-34s COPY FAILED\n' "$b"; rc=1; fi
    else
        printf '  %-34s not built (skipped)\n' "$b"
    fi
done

scp $SSHOPT -P 2222 "$DT/atrium-dispensatord" root@localhost:/usr/local/bin/ >/dev/null 2>&1 \
    && printf '  %-34s ok\n' atrium-dispensatord || { printf '  %-34s COPY FAILED\n' atrium-dispensatord; rc=1; }
g 'mkdir -p /usr/local/etc/atrium/devices.d' >/dev/null 2>&1

echo "=== rc.d scripts -> /usr/local/etc/rc.d ==="
for f in "$BSD/portcullis/jaild/etc/atrium-jaild" \
         "$BSD/portcullis/atrium-volumes/etc/atrium-volumes" \
         "$BSD/portcullis/portcullisd/etc/atrium-portcullisd-daemon" \
         "$BSD/portcullis/portcullisd/etc/atrium-portcullisd-bootstrap" \
         "$BSD/dispensator/etc/atrium-dispensatord"; do
    [ -f "$f" ] || { echo "  MISSING in tree: $f"; rc=1; continue; }
    scp $SSHOPT -P 2222 "$f" root@localhost:/usr/local/etc/rc.d/ >/dev/null 2>&1 \
        && printf '  %-34s ok\n' "$(basename "$f")" || { printf '  %-34s COPY FAILED\n' "$(basename "$f")"; rc=1; }
done

echo "=== devfs rulesets -> /usr/local/etc/atrium/devfs.rules ==="
# ★★ Before any daemon starts: jaild and portcullis refuse a jail whose devfs
# ruleset has no rules (portcullis.md §9.1b), so deploying the daemons without
# the rules would fail every service that asks for 20/21/22 — correctly, but
# for a reason this script can prevent. Loaded now and on every boot.
scp $SSHOPT -P 2222 "$BSD/etc/atrium.devfs.rules" root@localhost:/root/atrium.devfs.rules >/dev/null 2>&1 \
  && g 'mkdir -p /usr/local/etc/atrium &&
        install -m 644 /root/atrium.devfs.rules /usr/local/etc/atrium/devfs.rules &&
        sysrc -q devfs_rulesets="/etc/defaults/devfs.rules /etc/devfs.rules /usr/local/etc/atrium/devfs.rules" >/dev/null &&
        sysrc -q devfs_load_rulesets=NO >/dev/null &&
        { sh -c ". /etc/rc.subr; load_rc_config devfs; devfs_init_rulesets" || { echo "devfs_init_rulesets failed"; exit 1; }; } &&
        for id in 20 21 22 33; do [ -n "$(devfs rule -s $id show)" ] || { echo "ruleset $id is empty"; exit 1; }; done' >/tmp/devfsdeploy.log 2>&1 \
  && echo "  rulesets 20 21 22 33 loaded" \
  || { echo "  devfs rulesets FAILED to load:"; sed 's/^/      /' /tmp/devfsdeploy.log | tail -5; rc=1; }
echo "=== pf isolation -> /usr/local/etc/atrium/pf.conf ==="
# ★★ Before any daemon starts: jaild refuses a networked jail unless these
# rules are loaded (network.md §0) — without them an app reached the host.
scp $SSHOPT -P 2222 "$BSD/etc/atrium.pf" root@localhost:/root/atrium.pf >/dev/null 2>&1 \
  && g 'mkdir -p /usr/local/etc/atrium
E='"$EGRESS_IF"'
[ -n "$E" ] || E=$(route -n get default 2>/dev/null | awk '"'"'/interface:/{print $2}'"'"')
# No default route (a restricted QEMU net): the one interface with an IPv4
# address that is neither loopback nor a jail bridge is the egress.
[ -n "$E" ] || E=$(ifconfig -l inet | tr " " "\n" | grep -Ev "^(lo|atrium|bridge|epair)" | awk '"'"'{n++; i=$0} END{if (n == 1) print i}'"'"')
[ -n "$E" ] || { echo "no default route and not exactly one candidate interface: set EGRESS_IF"; exit 1; }
cur=$(sysrc -n pf_rules 2>/dev/null)
case "$cur" in ""|/etc/pf.conf|/usr/local/etc/atrium/pf.conf) ;;
  *) echo "pf_rules is $cur — an operator ruleset; add the atrium rules to it by hand"; exit 1;;
esac
[ "$cur" = /etc/pf.conf ] && [ -s /etc/pf.conf ] && { echo "/etc/pf.conf exists — an operator ruleset; add the atrium rules to it by hand"; exit 1; }
cp /root/atrium.pf /usr/local/etc/atrium/pf.conf
ifconfig "$E" group atrium_egress
printf "ifconfig %s group atrium_egress\\n" "$E" > "/etc/start_if.$E"
sysrc -q pf_enable=YES pf_rules=/usr/local/etc/atrium/pf.conf gateway_enable=YES >/dev/null
kldstat -q -m pf || kldload pf
sysctl -q net.inet.ip.forwarding=1 >/dev/null
pfctl -f /usr/local/etc/atrium/pf.conf 2>/dev/null && pfctl -e >/dev/null 2>&1
pfctl -s rules | grep -q "block drop in quick on atrium inet from any to (self)" || exit 1
pfctl -s rules | grep -q "anchor \"atrium/\*\"" || exit 1
pfctl -s rules | grep -qx "block drop in quick on atrium all" || exit 1
pfctl -s info | grep -q "Status: Enabled" || exit 1' >/tmp/pfdeploy.log 2>&1 \
  && echo "  pf enabled, isolation rules loaded, forwarding on (and at boot)" \
  || { echo "  pf isolation FAILED: $(tail -1 /tmp/pfdeploy.log)"; rc=1; }
echo "=== config -> /etc/atrium ==="
g 'mkdir -p /etc/atrium/services.d /var/db/atrium /var/log/atrium' >/dev/null 2>&1
# atrium-volumes' arcad calls (persona folders' keys, tessera-encryption.md
# E4.6) run in a child as this uid; arcad's ARCA_SERVICES names it.
g 'id _volumes-vk >/dev/null 2>&1 || pw useradd _volumes-vk -u 65053 -d /nonexistent -s /usr/sbin/nologin -c "atrium-volumes folder keys"' >/dev/null 2>&1 \
    && echo "  _volumes-vk (65053) ok" || { echo "  _volumes-vk FAILED"; rc=1; }
# ★ The TARGET's [resource_control] survives the deploy. Copying the repo
# policy over it dropped `allow_rctl = true`, and jaild then refused every
# limited one-shot (`rctl.disabled`) until it was re-added by hand
# (portcullis.md §6.5.2e′ snags). ALLOW_RCTL=true|false sets it explicitly.
tmpd=$(mktemp -d "${TMPDIR:-/tmp}/deploy-portcullis.XXXXXX")
if g 'cat /etc/atrium/jaild.policy.toml' > "$tmpd/live.toml" 2>/dev/null; then live="$tmpd/live.toml"; else live=-; fi
# ★ plain_app_data (mail.md §6.4a A1): portcullisd refuses every app launch
# whose data the kernel doesn't report in an open key group, unless this is
# true. This script deploys to dev VMs, so it defaults to true; the target's
# own value survives a deploy, and PLAIN_APP_DATA=true|false sets it. It is
# ignored wherever the kernel reports key groups, so it never unseals a
# machine that has them.
case "${PLAIN_APP_DATA:-}" in
    true|false) plain=$PLAIN_APP_DATA ;;
    '') plain=$(g 'sed -nE "s/^[[:space:]]*plain_app_data[[:space:]]*=[[:space:]]*(true|false).*/\\1/p" /etc/atrium/volumes.policy.toml' 2>/dev/null | head -1)
        [ -n "$plain" ] || plain=true ;;
    *) echo "PLAIN_APP_DATA must be true or false, not '$PLAIN_APP_DATA'" >&2; exit 2 ;;
esac
# A top-level key goes before the first table.
awk -v plain="$plain" '!done && /^\[/ { print "plain_app_data = " plain "\n"; done = 1 } { print }' \
    "$BSD/etc/volumes.policy.toml" > "$tmpd/volumes.policy.toml"
echo "  plain_app_data = $plain"
if sh "$BSD/scripts/jaild-policy-merge.sh" "$BSD/etc/jaild.policy.toml" "$live" \
        > "$tmpd/jaild.policy.toml" 2> "$tmpd/merge.log" \
   && scp $SSHOPT -P 2222 "$tmpd/jaild.policy.toml" "$tmpd/volumes.policy.toml" \
        root@localhost:/etc/atrium/ >/dev/null 2>&1; then
    echo "  policies ok ($(sed 's/^jaild-policy-merge: //' "$tmpd/merge.log"))"
else
    echo "  policies FAILED: $(cat "$tmpd/merge.log")"; rc=1
fi
scp $SSHOPT -P 2222 "$BSD"/etc/services.d/*.toml \
    root@localhost:/etc/atrium/services.d/ >/dev/null 2>&1 \
    && echo "  services.d ok ($(ls "$BSD"/etc/services.d/*.toml | wc -l | tr -d ' ') manifests)" \
    || { echo "  services.d FAILED"; rc=1; }

# The slow-fail smoke's exec target: sleep(1) under an atrium- name, so it
# satisfies jaild's exec allow-list and lives the ~3 s its budget model needs.
# Its manifest always promised this "setup-time" helper and nothing installed
# it, so every launch was an execve ENOENT.
g 'install -m 0755 /bin/sleep /usr/local/bin/atrium-slowfail' >/dev/null 2>&1 \
    && echo "  atrium-slowfail helper ok" || { echo "  atrium-slowfail helper FAILED"; rc=1; }

# The aqueduct attach smoke's SOURCE directory. Its manifest asks jaild to
# nullfs it into the jail; it was created by hand on the ZFS-era VM and never
# by anything in the tree, so on a fresh root the attach got all the way to
# nmount(2) and failed ENOENT. The marker lets a check confirm the content is
# visible through the jail-side mount, not just that a mount exists.
g 'mkdir -p /var/lib/atrium/storage/jails/atrium-attach-source &&
   echo "atrium aqueduct attach smoke source" \
     > /var/lib/atrium/storage/jails/atrium-attach-source/MARKER' >/dev/null 2>&1 \
    && echo "  aq attach-source dir ok" || { echo "  aq attach-source dir FAILED"; rc=1; }

g 'chmod 0755 /usr/local/etc/rc.d/atrium-* /usr/local/bin/atrium-* \
              /usr/local/bin/portcullisd /usr/local/bin/portcullis \
              /usr/local/bin/opifex 2>/dev/null' >/dev/null 2>&1

[ "$START" = 1 ] || { echo "=== install only (START=0) ==="; exit $rc; }

echo "=== enable + start (dependency order) ==="
g 'for s in atrium_jaild atrium_dispensatord atrium_volumes atrium_portcullisd_daemon; do
     sysrc ${s}_enable=YES >/dev/null 2>&1; done' >/dev/null 2>&1
for s in atrium-jaild atrium-dispensatord atrium-volumes atrium-portcullisd-daemon; do
    printf '  %-32s ' "$s"
    # ★ "already running" is SUCCESS on a re-run, not failure. service(8)
    # exits non-zero for it, so checking the process first is what makes this
    # script idempotent in its OUTPUT and not just in its effect — a deploy
    # script that prints FAILED on a healthy system trains you to ignore it.
    if running $s; then
        echo "already running"
    elif g "service $s start >/tmp/svc.log 2>&1"; then
        # daemon(8) answers before the daemon has run: a daemon that exits
        # at once (a policy it can't read, a socket in use) shows here.
        sleep 1
        if running $s; then echo OK; else
            echo "FAILED (started, then exited)"
            case $s in atrium-volumes) log=$s ;; *) log=${s#atrium-} ;; esac
            g "tail -5 /var/log/atrium/$log.log 2>/dev/null" | sed 's/^/      /'; rc=1
        fi
    else
        echo FAILED; g "tail -3 /tmp/svc.log" | sed 's/^/      /'; rc=1
    fi
    sleep 2
done

if [ "$BOOTSTRAP_WAS" = 1 ]; then
    printf '  %-32s ' atrium-portcullisd-bootstrap
    if g "service atrium-portcullisd-bootstrap start >/tmp/svc.log 2>&1"; then echo "OK (was running)"; else echo FAILED; rc=1; fi
fi

echo "=== verify ==="
g 'for p in atrium-jaild atrium-volumes atrium-portcullisd-daemon; do
     printf "  %-32s %s\n" "$p" "$(service $p status >/dev/null 2>&1 && echo RUNNING || echo DOWN)"
   done
   echo "  sockets: $(ls /var/run/atrium/*.sock 2>/dev/null | wc -l | tr -d " ")"'
g 'service atrium-jaild status >/dev/null && service atrium-volumes status >/dev/null \
   && service atrium-portcullisd-daemon status >/dev/null' || { echo "  ★ a daemon is DOWN"; rc=1; }

echo
echo "To drive the smoke manifests:  service atrium-portcullisd-bootstrap start"
echo "  (logs: /var/log/atrium/portcullisd-bootstrap.log, per-service logs alongside)"
exit $rc

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
#
#   sh scripts/deploy-portcullis.sh              # deploy + enable + start
#   START=0 sh scripts/deploy-portcullis.sh      # install only
set -u
BSD="$(cd "$(dirname "$0")/.." && pwd)"
T="$BSD/portcullis/target/aarch64-unknown-freebsd/release"
KEY="$HOME/.ssh/fresco_bsd_ed25519"
START=${START:-1}
SSHOPT="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
g() { ssh $SSHOPT -p 2222 root@localhost "$@"; }

# ── build anything missing ──────────────────────────────────────────
# ★ Cross-build on the HOST, never in the VM (feedback_always_cross_build).
if [ ! -x "$T/atrium-jaild" ] || [ ! -x "$T/atrium-volumes" ]; then
    echo "=== building missing daemons (cross) ==="
    ( cd "$BSD/portcullis" && TESSERA_CORE_LIB="$BSD/atrium-tessera/core" \
        cargo build --release --target aarch64-unknown-freebsd \
        -p jaild -p atrium-volumes -p portcullisd ) || {
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
for s in atrium-portcullisd-daemon atrium-volumes atrium-jaild; do
    if g "pgrep -f $s >/dev/null 2>&1"; then
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

echo "=== rc.d scripts -> /usr/local/etc/rc.d ==="
for f in "$BSD/portcullis/jaild/etc/atrium-jaild" \
         "$BSD/portcullis/atrium-volumes/etc/atrium-volumes" \
         "$BSD/portcullis/portcullisd/etc/atrium-portcullisd-daemon" \
         "$BSD/portcullis/portcullisd/etc/atrium-portcullisd-bootstrap"; do
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
        service devfs restart >/dev/null &&
        for id in 20 21 22; do [ -n "$(devfs rule -s $id show)" ] || exit 1; done' >/dev/null 2>&1 \
  && echo "  rulesets 20 21 22 loaded" || { echo "  devfs rulesets FAILED to load"; rc=1; }
echo "=== pf isolation -> /usr/local/etc/atrium/pf.conf ==="
# ★★ Before any daemon starts: jaild refuses a networked jail unless these
# rules are loaded (network.md §0) — without them an app reached the host.
scp $SSHOPT -P 2222 "$BSD/etc/atrium.pf" root@localhost:/root/atrium.pf >/dev/null 2>&1 \
  && g 'mkdir -p /usr/local/etc/atrium
E=$(route -n get default 2>/dev/null | awk '"'"'/interface:/{print $2}'"'"')
[ -n "$E" ] || { echo "no default route: cannot pick the NAT interface"; exit 1; }
cur=$(sysrc -n pf_rules 2>/dev/null)
case "$cur" in ""|/etc/pf.conf|/usr/local/etc/atrium/pf.conf) ;;
  *) echo "pf_rules is $cur — an operator ruleset; add the atrium rules to it by hand"; exit 1;;
esac
[ "$cur" = /etc/pf.conf ] && [ -s /etc/pf.conf ] && { echo "/etc/pf.conf exists — an operator ruleset; add the atrium rules to it by hand"; exit 1; }
sed "s/EXT_IF/$E/" /root/atrium.pf > /usr/local/etc/atrium/pf.conf
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
scp $SSHOPT -P 2222 "$BSD/etc/jaild.policy.toml" "$BSD/etc/volumes.policy.toml" \
    root@localhost:/etc/atrium/ >/dev/null 2>&1 && echo "  policies ok" || { echo "  policies FAILED"; rc=1; }
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
g 'for s in atrium_jaild atrium_volumes atrium_portcullisd_daemon; do
     sysrc ${s}_enable=YES >/dev/null 2>&1; done' >/dev/null 2>&1
for s in atrium-jaild atrium-volumes atrium-portcullisd-daemon; do
    printf '  %-32s ' "$s"
    # ★ "already running" is SUCCESS on a re-run, not failure. service(8)
    # exits non-zero for it, so checking the process first is what makes this
    # script idempotent in its OUTPUT and not just in its effect — a deploy
    # script that prints FAILED on a healthy system trains you to ignore it.
    if g "pgrep -f $s >/dev/null 2>&1"; then
        echo "already running"
    elif g "service $s start >/tmp/svc.log 2>&1"; then
        echo OK
    else
        echo FAILED; g "tail -3 /tmp/svc.log" | sed 's/^/      /'; rc=1
    fi
    sleep 2
done

echo "=== verify ==="
g 'for p in atrium-jaild atrium-volumes atrium-portcullisd-daemon; do
     printf "  %-32s %s\n" "$p" "$(pgrep -f $p >/dev/null && echo RUNNING || echo DOWN)"
   done
   echo "  sockets: $(ls /var/run/atrium/*.sock 2>/dev/null | wc -l | tr -d " ")"'
g 'pgrep -f atrium-jaild >/dev/null && pgrep -f atrium-volumes >/dev/null \
   && pgrep -f atrium-portcullisd-daemon >/dev/null' || { echo "  ★ a daemon is DOWN"; rc=1; }

echo
echo "To drive the smoke manifests:  service atrium-portcullisd-bootstrap start"
echo "  (logs: /var/log/atrium/portcullisd-bootstrap.log, per-service logs alongside)"
exit $rc

#!/bin/sh
# Commission atrium-net and atrium-netconf as system services
# (docs/spec/atrium-net.md §3, §10.6). Runs IN the FreeBSD guest, as root,
# once per host:
#
#   sh atrium-net-service.sh DIR
#
# DIR holds the binaries atrium-net, atrium-lease, atrium-probe,
# atrium-netconf, atrium-netctl and the rc.d scripts atrium-net and
# atrium-netconf (from atrium-net/etc). Creates the users, installs the
# binaries and scripts, sets up local-unbound (the system resolver; it makes
# /etc/resolv.conf point at 127.0.0.1), and enables both services. Starting
# them is left to the caller or the next boot.
#
# Ethernet ports keep whatever rc.conf says about them: atrium-net takes only
# the ones rc.conf leaves unconfigured or just "up" (atrium_net_ifaces="AUTO";
# ifconfig_DEFAULT counts for every port without its own line). To hand a port
# over: `sysrc ifconfig_em0=up`, or name ports in atrium_net_ifaces.
set -e
D=${1:?usage: $0 DIR}
for f in atrium-net atrium-lease atrium-probe atrium-netconf atrium-netctl; do
    [ -x "$D/$f" ] || { echo "missing $D/$f"; exit 2; }
done

id _atrium-lease >/dev/null 2>&1 || pw useradd _atrium-lease -u 65034 -d /nonexistent -s /usr/sbin/nologin -c 'atrium-net lease parser'
id _atrium-probe >/dev/null 2>&1 || pw useradd _atrium-probe -u 65035 -d /nonexistent -s /usr/sbin/nologin -c 'atrium-net connectivity probe'
id _atrium-netconf >/dev/null 2>&1 || pw useradd _atrium-netconf -u 65036 -d /nonexistent -s /usr/sbin/nologin -c 'atrium-net settings front'
# atrium-lease and atrium-probe chroot here.
install -d -m 0555 /var/empty

install -m 0755 "$D/atrium-net" "$D/atrium-netconf" "$D/atrium-netctl" /usr/local/bin/
install -d /usr/local/libexec
install -m 0755 "$D/atrium-lease" "$D/atrium-probe" /usr/local/libexec/
install -d /usr/local/etc/rc.d
install -m 0555 "$D/rc.d/atrium-net" /usr/local/etc/rc.d/atrium-net
install -m 0555 "$D/rc.d/atrium-netconf" /usr/local/etc/rc.d/atrium-netconf
install -d -m 0700 /var/db/atrium-net

# local-unbound-setup also makes resolvconf leave /etc/resolv.conf alone,
# so dhclient on a port rc.conf keeps cannot take the resolver back.
if [ ! -f /var/unbound/unbound.conf ] || ! grep -q '^nameserver 127.0.0.1' /etc/resolv.conf; then
    local-unbound-setup >/dev/null
fi
sysrc local_unbound_enable=YES atrium_net_enable=YES atrium_netconf_enable=YES >/dev/null
echo "atrium-net commissioned: $(rcorder /etc/rc.d/* /usr/local/etc/rc.d/* 2>/dev/null | grep -nE '/(local_unbound|atrium-net|atrium-netconf)$' | sed 's|.*/||' | tr '\n' ' ')"

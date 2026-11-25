#!/bin/sh
# The wired 802.1X port's switch side (scripts/aura-ap-vm.sh wired): like a
# switch that carries traffic only on an authorized port, DHCP is served on
# the cable only while hostapd has the port authorized. hostapd's
# "authorizing port" and "unauthorizing port" lines are piped in
# (aura-ap-vm.sh wired); it logs them as it opens or shuts the port; a
# re-authentication that fails shuts it with no AP-STA-DISCONNECTED event,
# so hostapd_cli's events are not enough. Only those lines reach this loop
# (a shell reading all of hostapd's -dd output falls seconds behind under
# TCG), and udhcpd is killed by pid: the client asks again for an address
# within a second of a failure.
pid=""
while IFS= read -r l; do
    case "$l" in
    *"IEEE 802.1X: authorizing port"*)
        [ -n "$pid" ] && kill "$pid" 2>/dev/null
        busybox udhcpd -f /tmp/udhcpd-wired.conf >>/tmp/udhcpd-wired.log 2>&1 &
        pid=$!
        echo "$(date +%s) open" >> /tmp/wired-port.log ;;
    *"IEEE 802.1X: unauthorizing port"*)
        [ -n "$pid" ] && kill "$pid" 2>/dev/null
        pid=""
        echo "$(date +%s) closed" >> /tmp/wired-port.log ;;
    esac
done
[ -n "$pid" ] && kill "$pid" 2>/dev/null

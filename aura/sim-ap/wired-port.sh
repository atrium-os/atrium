#!/bin/sh
# hostapd_cli -a for the wired 802.1X port (scripts/aura-ap-vm.sh wired):
# like a switch that opens the port only after the sign-in, DHCP is
# served on the cable only while a station is authorized.
# Called as: wired-port.sh IFACE EVENT ADDR
case "$2" in
AP-STA-CONNECTED)
    pkill -f '[u]dhcpd -f /tmp/udhcpd-wired.conf'
    nohup busybox udhcpd -f /tmp/udhcpd-wired.conf >>/tmp/udhcpd-wired.log 2>&1 &
    echo "$(date +%s) open $3" >> /tmp/wired-port.log ;;
AP-STA-DISCONNECTED)
    pkill -f '[u]dhcpd -f /tmp/udhcpd-wired.conf'
    echo "$(date +%s) closed $3" >> /tmp/wired-port.log ;;
esac

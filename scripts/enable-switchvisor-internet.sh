#!/bin/sh
# Share the current macOS IPv4 uplink with Switchvisor's USB subnet.
set -eu

if [ "$(id -u)" -ne 0 ]; then
    echo "Run with sudo: $0" >&2
    exit 1
fi
uplink=$(/sbin/route -n get default | awk '/interface:/ {print $2}')
case "$uplink" in
    ''|*[!a-zA-Z0-9]*) echo 'Invalid default-route interface' >&2; exit 1 ;;
esac
usb=''
for candidate in $(/sbin/ifconfig -l); do
    if /sbin/ifconfig "$candidate" | grep -q 'ether 02:53:56:00:00:01'; then
        usb=$candidate
        break
    fi
done
if [ -z "$usb" ] || [ "$usb" = "$uplink" ]; then
    echo 'Switchvisor USB interface is missing or owns the default route' >&2
    exit 1
fi
/sbin/ifconfig "$usb" | grep -q 'inet 192.168.77.2 ' || {
    echo 'Switchvisor USB must have address 192.168.77.2 first' >&2
    exit 1
}
root_nat=$(/sbin/pfctl -sn)
printf '%s\n' "$root_nat" | grep -Fq 'nat-anchor "com.apple/*"' || {
    echo 'Existing Apple PF NAT anchor is missing; no settings changed' >&2
    exit 1
}
# Never reload /etc/pf.conf or replace the root ruleset. Only this dedicated
# anchor belongs to Switchvisor. The rule follows the current uplink.
rule="nat on $uplink inet from 192.168.77.0/24 to any -> ($uplink)"
printf '%s\n' "$rule" | /sbin/pfctl -a com.apple/switchvisor -nf -
printf '%s\n' "$rule" | /sbin/pfctl -a com.apple/switchvisor -f -
if /sbin/pfctl -s info 2>/dev/null | grep -q '^Status: Disabled'; then
    /sbin/pfctl -E
fi
/usr/sbin/sysctl -w net.inet.ip.forwarding=1
/sbin/pfctl -a com.apple/switchvisor -sn
printf 'Switchvisor Internet: %s -> %s\n' "$usb" "$uplink"

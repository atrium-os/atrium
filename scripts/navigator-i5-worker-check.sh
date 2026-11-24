#!/bin/sh
# Run inside FreeBSD while the installed Navigator engine is rendering its fixture.
# Supply the expected caps from the signed test manifest, in MiB and % of one CPU.
set -eu
[ "$#" = 3 ] || { echo "usage: $0 WORKER_PID MEMORY_MIB CPU_PERCENT" >&2; exit 2; }
pid=$1
jid=$(ps -o jid= -p "$pid" | tr -d ' ')
name=$(jls -j "$jid" name)
case "$name" in app-org-atrium-navigator-engine--*) ;; *) exit 1;; esac
jls -j "$jid" -n jid name ip4 ip6 | grep 'ip4=disable ip6=disable'
procstat -s "$pid" | awk -v p="$pid" '$1 == p && $3 != 0 && $10 ~ /C/ { print; ok=1 } END { exit !ok }'
rules=$(rctl "jail:$name")
# rctl uses raw bytes by default.
echo "$rules" | grep -Fx "jail:$name:memoryuse:sigkill=$(($2 * 1024 * 1024))"
echo "$rules" | grep -Fx "jail:$name:pcpu:deny=$3"
shared=$(procstat -x "$pid" | awk '$3 == "AT_TIMEKEEP" {print $4}')
test -n "$shared"
# sh compares hexadecimal addresses without awk's implementation-specific strtonum.
procstat -v "$pid" | (
    seen_file=0
    seen_shared=0
    while read -r process start end prot res pres ref shd flags type path; do
        [ "$process" = "$pid" ] || continue
        case "$prot" in *x*) ;; *) continue ;; esac
        case "$prot" in *w*) echo "writable executable mapping at $start" >&2; exit 1 ;; esac
        case "$type:$path" in
            vn:/var/lib/atrium/apps/org.atrium.navigator.engine/bin/counter) seen_file=1 ;;
            vn:/var/lib/atrium/apps/org.atrium.navigator.engine/*) ;;
            ph:) [ "$((start))" -le "$((shared))" ] && [ "$((shared))" -lt "$((end))" ] || exit 1
                 seen_shared=$((seen_shared + 1)) ;;
            *) echo "unexpected executable mapping $type $path" >&2; exit 1 ;;
        esac
    done
    [ "$seen_file" = 1 ] && [ "$seen_shared" = 1 ]
)
# Detect worker exit or replacement across the independent kernel queries.
[ "$(ps -o jid= -p "$pid" | tr -d ' ')" = "$jid" ]
echo "Worker $pid in jail $jid passed the executable mapping checks."

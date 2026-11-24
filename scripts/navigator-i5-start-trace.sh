#!/bin/sh
# Line up one island Start across navigatord, limend, portcullisd, the
# one-shot lane and jaild (navigator-islands.md "I5 Start trace").
#
# Each of those prints `start-trace WHO STAGE mono_us=N pid=P TAG` on stderr
# for an engine launch only, on CLOCK_MONOTONIC, so the lines of every
# process (and every jail) share one clock. Pass the daemons' log files,
# taken after one press; the output lists the stages in time order with the
# time since the first and since the previous stage.
#
#   sh navigator-i5-start-trace.sh navigatord.log limend.log portcullisd.log jaild.log
#
# With --last, only the lines from the last `navigatord widget-start` on are
# used, so a log with several presses gives the most recent one.
set -eu
last=0
[ "${1:-}" = --last ] && { last=1; shift; }
[ "$#" -ge 1 ] || { echo "usage: $0 [--last] LOG..." >&2; exit 2; }
for f in "$@"; do [ -r "$f" ] || { echo "$0: cannot read $f" >&2; exit 2; }; done
lines=$(grep -h '^start-trace ' "$@" | awk '{
    us = ""; for (i = 1; i <= NF; i++) if ($i ~ /^mono_us=/) us = substr($i, 9)
    if (us == "") { print "malformed: " $0 > "/dev/stderr"; bad = 1; next }
    print us, $0
} END { exit bad }' | sort -n -k1,1)
[ -n "$lines" ] || { echo "$0: no start-trace lines in $*" >&2; exit 1; }
if [ "$last" = 1 ]; then
    start=$(echo "$lines" | awk '$3 == "navigatord" && $4 == "widget-start" { s = $1 } END { print s }')
    [ -n "$start" ] || { echo "$0: no navigatord widget-start line" >&2; exit 1; }
    lines=$(echo "$lines" | awk -v s="$start" '$1 >= s')
fi
echo "$lines" | awk '
    NR == 1 { first = $1; prev = $1 }
    {
        tag = ""; for (i = 7; i <= NF; i++) tag = tag " " $i
        printf "%9.3f ms  +%8.3f ms  %-11s %-15s %s%s\n", ($1 - first) / 1000, ($1 - prev) / 1000, $3, $4, $6, tag
        prev = $1; n++
    }
    END { printf "%d stages, %.3f ms first to last\n", n, (prev - first) / 1000 }'
# Every hop must have printed, or the trace is not the installed path.
for want in "navigatord widget-start" "limend request-island" "portcullisd exec-begin" \
            "oneshot jaild-call" "jaild create-begin" "jaild pdfork-done" \
            "limend embed-handed" "navigatord island-fetch" "navigatord attached"; do
    echo "$lines" | awk -v w="$want" '($3 " " $4) == w { f = 1 } END { exit !f }' \
        || { echo "MISSING stage: $want" >&2; missing=1; }
done
[ -z "${missing:-}" ]

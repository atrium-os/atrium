#!/bin/sh
# Verify one completed physical signing attempt using its own audit slice.
set -eu
audit=$1 output=$2 message=$3 allowed=$4 decision=$5
msg=${message##*/}
awk -v decision="$decision" '
    /: start key=/ && /ArcaSshSign/ && /workspace keys: physical-touch-fixture/ { key=$3; starts++ }
    $2 == "end" && $3 == key ":" && index($0, ": " decision) { ends++ }
    END { exit !(starts == 1 && ends == 1) }
' "$audit"
case "$decision" in
    'cancelled on the sheet')
        # workspace exec returns success after streaming a refused sign request.
        grep -Fq "Signing $msg failed: agent refused operation" "$output"
        test ! -e "$message.sig"
        ;;
    'verified by security key')
        ssh-keygen -Y verify -f "$allowed" -I alice -n file -s "$message.sig" < "$message"
        ;;
    *) exit 2 ;;
esac

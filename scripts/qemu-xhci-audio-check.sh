#!/bin/sh
# Does qemu's xHCI carry USB audio exactly? (qemu-xhci-isoch.md §4 A)
#
# No real USB: qemu's own usb-audio device (full speed, UAC1, 48 kHz S16) on
# the guest's xHCI, and for control the same device on UHCI and the HDA card, each writing what the
# guest played to a wav file on the host. lyra-play's counter signal goes
# through lyrad bit-perfect to each card in turn; every frame must come out
# exactly (scripts/lib/audio-counter-check.py), on lyrad's side and on the
# device's.
#
#   QEMU_DIR=<qemu tree with the wavaudio.c fix> \
#   QEMU_EXTRA="$(sh scripts/qemu-xhci-audio-check.sh qemu-args)" sh scripts/run-vm.sh
#   sh scripts/qemu-xhci-audio-check.sh            # with the VM up (ask first: shared VM)
#
# SECS (default 30) per card. OUT (default ~/atrium-vm/xhci-audio) holds the
# wav files; qemu writes them, so the run must be on the Mac that runs it.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${OUT:-$HOME/atrium-vm/xhci-audio}"
if [ "${1:-}" = qemu-args ]; then
    mkdir -p "$OUT"
    rm -f "$OUT/usb-audio.wav" "$OUT/usb-uhci.wav" "$OUT/hda.wav"
    # out.mixing-engine=off: each device's samples go to its wav file as they
    # are (qemu's float mixer truncates: s16 -3 came out -2). Needs our fork's
    # wavaudio.c fix: upstream the file stays at the audiodev's 44.1 kHz
    # default, so a 48 kHz device drains too slowly and drops 1 ms packets
    # (8%), which looks like a transport fault.
    echo "-device qemu-xhci,id=xhci" \
         "-audiodev wav,id=usbsnd,path=$OUT/usb-audio.wav,out.mixing-engine=off" \
         "-device usb-audio,bus=xhci.0,audiodev=usbsnd" \
         "-device ich9-usb-uhci1,id=uhci" \
         "-audiodev wav,id=uhcisnd,path=$OUT/usb-uhci.wav,out.mixing-engine=off" \
         "-device usb-audio,bus=uhci.0,audiodev=uhcisnd" \
         "-audiodev wav,id=hdasnd,path=$OUT/hda.wav,out.mixing-engine=off" \
         "-device intel-hda -device hda-output,audiodev=hdasnd"
    exit 0
fi
VSSH="$ROOT/scripts/vssh"
"$VSSH" true >/dev/null 2>&1 || { echo "the VM is not up (see the usage above)" >&2; exit 1; }
"$VSSH" 'kldload -n snd_hda snd_uaudio; sleep 3' >/dev/null 2>&1
units=$("$VSSH" 'for p in $(sysctl -N dev.pcm 2>/dev/null | sed -n "s/^dev\.pcm\.\([0-9]*\)\.%parent$/\1/p" | sort -un); do
    echo "$p $(sysctl -n dev.pcm.$p.%parent)"; done')
echo "$units"
# Which controller each USB card hangs off: pcm -> uaudio -> uhub -> usbus -> xhci/uhci.
ctl() { "$VSSH" "p=\$(sysctl -n dev.pcm.$1.%parent); u=\$(sysctl -n dev.\${p%%[0-9]*}.\${p##*[a-z]}.%parent);
    b=\$(sysctl -n dev.uhub.\${u##*[a-z]}.%parent); sysctl -n dev.usbus.\${b##*[a-z]}.%parent"; }
hda=$(echo "$units" | awk '$2 ~ /^hdaa/ {print $1; exit}')
usb=""; uhci=""
for p in $(echo "$units" | awk '$2 ~ /^uaudio/ {print $1}'); do
    case $(ctl "$p") in xhci*) usb=$p ;; uhci*) uhci=$p ;; esac
done
[ -n "$hda" ] && [ -n "$usb" ] && [ -n "$uhci" ] || { echo "want HDA, xHCI and UHCI cards (boot with qemu-args)" >&2; exit 1; }
fails=0
echo "=== control: HDA (pcm$hda) -> $OUT/hda.wav"
SIGNAL=counter DAC_UNITS=$hda WAV="$OUT/hda.wav" TONE_SECS="${SECS:-30}" sh "$ROOT/scripts/usb-dac-check.sh" || fails=$((fails + 1))
echo "=== qemu usb-audio on xHCI (pcm$usb) -> $OUT/usb-audio.wav"
SIGNAL=counter DAC_UNITS=$usb WAV="$OUT/usb-audio.wav" TONE_SECS="${SECS:-30}" sh "$ROOT/scripts/usb-dac-check.sh" || fails=$((fails + 1))
echo "=== control: qemu usb-audio on UHCI (pcm$uhci) -> $OUT/usb-uhci.wav"
SIGNAL=counter DAC_UNITS=$uhci WAV="$OUT/usb-uhci.wav" TONE_SECS="${SECS:-30}" sh "$ROOT/scripts/usb-dac-check.sh" || fails=$((fails + 1))
[ $fails = 0 ] && echo "PASS: every path carried every frame" || { echo "FAIL: $fails path(s)"; exit 1; }

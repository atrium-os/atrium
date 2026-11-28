#!/bin/sh
# Run against an explicitly commissioned, disposable B5 guest over its SSH wrapper.
set -eu
root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
. "$root/scripts/lib/gate.sh"
gate_begin vinculum-a2dp-jailed 7
: "${B5_SSH:?path to an SSH wrapper for the disposable B5 guest}"
: "${B5_HCI:?Bumble transport on the same simulated radio as vinculumd}"
: "${B5_RESULTS:?new directory for gate evidence}"
mkdir -m 700 "$B5_RESULTS"
python=${VINCULUM_VENV:-$HOME/.cache/vinculum-venv}/bin/python
"${FFMPEG:-ffmpeg}" -version >/dev/null 2>&1 || gate_abort "independent FFmpeg decoder unavailable"
g() { "$B5_SSH" "$1"; }
ctl() { g "su -m _vinculum -c '/usr/local/bin/vinculum-ctl -s /atrium/sockets/bluetooth-control/control.sock $*'"; }
wait_line() {
    n=0
    until grep -q "$1" "$2"; do
        [ "$n" -lt 100 ] && kill -0 "$peer" 2>/dev/null || return 1
        sleep .2; n=$((n+1))
    done
}
g 'test -d /root/b5-check && jls -n jid name ip4 ip6 && ps -axo jid,uid,args | awk '\''$1 ~ /^[1-9][0-9]*$/ '\''' > "$B5_RESULTS/services"
for entry in 65016:lyrad 65017:choragusd 65024:vinculumd 65026:vinculum-audio; do
    jid=$(sed -n "s/^jid=\([0-9]*\) name=atrium-${entry#*:} ip4=disable ip6=disable$/\1/p" "$B5_RESULTS/services")
    [ -n "$jid" ] && grep -Eq "^[[:space:]]*$jid[[:space:]]+${entry%:*}[[:space:]]+([^ ]*/)?atrium-${entry#*:} " "$B5_RESULTS/services" || gate_abort "missing jailed ${entry#*:}"
done
ok "four service jails run under their assigned uids"
guest_logs=$(g 'mktemp -d /root/b5-check/players.XXXXXX')
peer=
trap 'if [ -n "$peer" ]; then kill "$peer" 2>/dev/null || true; wait "$peer" 2>/dev/null || true; fi' EXIT
for rate in 48000 44100; do
    log=$B5_RESULTS/sink-$rate.log
    "$python" -u "$root/vinculum/harness/peer_a2dp_jailed.py" "$B5_HCI" "$rate" "$B5_RESULTS/capture-$rate" > "$log" 2>&1 &
    peer=$!
    wait_line '^Classic address ' "$log" || gate_abort "sink startup ($log)"
    addr=$(awk '/^Classic address /{print $3;exit}' "$log")
    echo "$addr" | grep -Eq '^([[:xdigit:]]{2}:){5}[[:xdigit:]]{2}$' || gate_abort "invalid simulated address"
    if ctl audio "$addr" on > "$B5_RESULTS/unbonded-$rate" 2>&1; then gate_abort "unbonded grant succeeded"; fi
    grep -q 'pair the Classic device first' "$B5_RESULTS/unbonded-$rate" || gate_abort "unexpected grant failure"
    ctl pair "$addr" --classic --yes --timeout 20 > "$B5_RESULTS/pair-$rate" || gate_abort "pairing"
    grep -q 'authenticated' "$B5_RESULTS/pair-$rate" || gate_abort "unauthenticated bond"
    sleep 1
    if grep -q '^AVDTP connected' "$log"; then gate_abort "AVDTP opened before consent"; fi
    ok "$rate Hz sink paired without receiving an AVDTP channel"
    ctl audio "$addr" on || gate_abort "grant"
    wait_line '^AVDTP started' "$log" || gate_abort "AVDTP start ($log)"
    g "cd /root/b5-check && for ch in 0 1; do hz=440; [ \"\$ch\" = 0 ] || hz=880; daemon -u b5player -o $guest_logs/player-$rate-\$ch.log env LYRA_PLAY_ONLY=\$ch LYRA_PLAY_FREQ=\$hz LYRA_PLAY_RATE=$rate /usr/local/bin/lyra-play /atrium/sockets/audio/audio.sock media 4 </dev/null >/dev/null 2>&1; done" || gate_abort "start players"
    sleep 1
    touch "$B5_RESULTS/capture-$rate"
    wait_line '^Tone verified' "$log" || gate_abort "decoded tones ($log)"
    ok "$rate Hz stereo music through choragusd, lyrad and the jailed profile"
    g "for i in 1 2 3 4 5 6 7 8; do if grep -q 'lyra-play: done' $guest_logs/player-$rate-0.log && grep -q 'lyra-play: done' $guest_logs/player-$rate-1.log; then cat $guest_logs/player-$rate-*.log; exit 0; fi; sleep 1; done; exit 1" > "$B5_RESULTS/players-$rate" || gate_abort "players did not drain"
    before=$(g 'wc -l < /var/log/atrium/atrium-choragusd.log')
    ctl audio "$addr" off || gate_abort "revoke"
    wait_line '^Media channel released' "$log" || gate_abort "revocation left the media channel open"
    g "for i in 1 2 3 4 5; do tail -n +$((before+1)) /var/log/atrium/atrium-choragusd.log | grep -E 'output .* gone' && exit 0; sleep 1; done; exit 1" > "$B5_RESULTS/removal-$rate" || gate_abort "Choragus kept revoked output"
    wait "$peer" || gate_abort "Bumble sink failed"
    peer=
    ctl remove "$addr" || gate_abort "remove test bond"
    ok "$rate Hz revocation released the media channel"

done
gate_end

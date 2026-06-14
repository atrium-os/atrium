#!/bin/sh
# seat-demo.sh — multi-user audio through the seat primitive (runs IN the VM).
#
# Two apps, owned by two human sessions (alice, bob), each feeding a distinct
# tone through the ONE front door (choragusd) into the ONE engine (lyrad). The
# seat says which session is bound to the engine; choragusd is session-aware, so
# only the ACTIVE session's audio reaches lyrad. Flip the seat and the audio
# follows — proven by spectrally splitting the dump into a before/after window.
#
#   active = alice   ->  player-a (500 Hz) plays, player-b (700 Hz) muted
#   atrium-seat set bob
#   active = bob     ->  player-a muted,         player-b (700 Hz) plays
set -e

REL=/mnt/host
CHOR=$REL/choragus/target/aarch64-unknown-freebsd/release/choragusd
SEAT=$REL/portcullis/target/aarch64-unknown-freebsd/release/atrium-seat
LYRAD=$REL/lyra/target/aarch64-unknown-freebsd/release/lyrad
PLAY=$REL/lyra/target/aarch64-unknown-freebsd/release/lyra-play
DUMP=$REL/lyra/seat-dump.raw
CLOG=/tmp/choragusd-seat.log

echo "== setup: per-app uids, launch registry, per-user grants, seat dir =="
pw useradd app-a -u 50000 -d /nonexistent -s /bin/sh 2>/dev/null || true
pw useradd app-b -u 50001 -d /nonexistent -s /bin/sh 2>/dev/null || true
mkdir -p /var/run/atrium /var/db/atrium/alice /var/db/atrium/bob

# Portcullis launch registry: uid -> (owning HUMAN session, app-id).
cat > /var/run/atrium/app-registry <<'EOF'
50000 alice org.atrium.player-a
50001 bob   org.atrium.player-b
EOF

# per-user grants (the authoritative Portcullis store). alice grants her player
# audio; bob grants his. Cross-app/cross-user is default-deny.
cat > /var/db/atrium/alice/policy.toml <<'EOF'
[grants."org.atrium.player-a"]
manifest_hash = "sha256:0"
granted_at    = "2026-06-14T00:00:00Z"
[grants."org.atrium.player-a".capabilities]
audio = true
EOF
cat > /var/db/atrium/bob/policy.toml <<'EOF'
[grants."org.atrium.player-b"]
manifest_hash = "sha256:0"
granted_at    = "2026-06-14T00:00:00Z"
[grants."org.atrium.player-b".capabilities]
audio = true
EOF

echo "== login establishes the active session: alice =="
$SEAT set alice
echo "   active session = $($SEAT active)"

echo "== start the engine (lyrad) — one DAC, seat-shared =="
rm -f /tmp/lyrad.ctl /tmp/lyrad.ctl.data
LYRA_DUMP=$DUMP $LYRAD --control /tmp/lyrad.ctl 12 &
LYRAD_PID=$!
# wait for the control + data sockets
i=0; while [ ! -S /tmp/lyrad.ctl.data ] && [ $i -lt 50 ]; do sleep 0.1; i=$((i+1)); done

echo "== start the session-aware policy daemon (choragusd --seat) =="
rm -f /tmp/choragus.sock
$CHOR --daemon /tmp/choragus.sock /tmp/lyrad.ctl \
      --seat \
      --app-registry /var/run/atrium/app-registry \
      --portcullis-grants /var/db/atrium > $CLOG 2>&1 &
CHOR_PID=$!
i=0; while [ ! -S /tmp/choragus.sock ] && [ $i -lt 50 ]; do sleep 0.1; i=$((i+1)); done

echo "== two apps register through the front door, as their own uids =="
su -m app-a -c "env LYRA_PLAY_FREQ=500 $PLAY /tmp/choragus.sock media 10 org.atrium.player-a" &
su -m app-b -c "env LYRA_PLAY_FREQ=700 $PLAY /tmp/choragus.sock media 10 org.atrium.player-b" &

echo "== WINDOW 1 (active=alice): expect 500 Hz only =="
sleep 4

echo "== seat switch: bind bob's session =="
$SEAT set bob
echo "   active session = $($SEAT active)"

echo "== WINDOW 2 (active=bob): expect 700 Hz only — audio follows the seat =="
sleep 4

wait $LYRAD_PID 2>/dev/null || true
kill $CHOR_PID 2>/dev/null || true

echo
echo "== choragusd gating decisions =="
grep -E "seat-aware|not active|re-gating|active session|registered -> stream" $CLOG || true
echo
echo "== dump: $DUMP ($(stat -f %z $DUMP 2>/dev/null) bytes) =="
echo "DONE"

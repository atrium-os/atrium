#!/bin/sh
# Provision only a fresh disposable guest. See bluetooth.md §15.1 for staging.
set -eu
[ "$(id -u)" = 0 ] && [ "$(pwd -P)" = /root/b5-check ] && [ -z "$(jls -q name)" ]
: "${B5_HCI_PORT:?guest loopback port forwarded to Rootcanal}"
case "$B5_HCI_PORT" in ''|*[!0-9]*) exit 2;; esac
[ "$B5_HCI_PORT" -gt 0 ] && [ "$B5_HCI_PORT" -le 65535 ]
# mkdir also guards against overwriting a previous gate's evidence or state.
mkdir -m 700 services
for entry in _vinculum:65024 _vinculum-audio:65026 _lyra:65016 _choragus:65017 b5player:51421; do
    n=${entry%:*}; u=${entry#*:}
    id "$n" >/dev/null 2>&1 || pw useradd "$n" -u "$u" -d /nonexistent -s /usr/sbin/nologin
    [ "$(id -u "$n")" = "$u" ]
done
for b in vinculumd vinculum-audio lyrad choragusd; do install -m 0755 "$b" "/usr/local/bin/atrium-$b"; done
install -m 0755 lyra-play vinculum-ctl /usr/local/bin
cp 35-atrium-lyrad.toml 36-atrium-choragusd.toml 47-atrium-vinculum-audio.toml services/
sed 's|"--arca", "/atrium/sockets/arca/arcad.sock"|"--keys", "/var/db/vinculum/keys", "--machine-key", "/var/db/vinculum/machine.key"|' \
    46-atrium-vinculumd.toml > services/46-atrium-vinculumd.toml
for d in audio audio-capture audio-monitor audio-control media-control audio-device audio-attribution; do
    install -d -m 0755 -o _choragus "/atrium/sockets/$d"
done
install -d -m 0750 -o _lyra -g _choragus /atrium/sockets/lyrad
install -d -m 0700 -o _choragus /var/db/choragus
install -d -m 0755 -o _vinculum /atrium/sockets/bluetooth-control
install -d -m 0700 -o _vinculum /atrium/sockets/bluetooth-hid /var/db/vinculum
install -d -m 0750 -o root -g _vinculum-audio /atrium/sockets/bluetooth-audio
install -d -m 0750 -o root -g _vinculum /atrium/sockets/vinculum-controllers
install -d /atrium/sockets/arca /atrium/sockets/ostiarius /var/lib/atrium/apps /var/db/atrium/b5player /var/run/atrium
echo b5player > /atrium/sockets/ostiarius/active-session
echo '51421 b5player org.test.b5' > /var/run/atrium/app-registry
cat > /var/db/atrium/b5player/policy.toml <<'POLICY'
[grants."org.test.b5"]
manifest_hash = "test"
granted_at = "test"
[grants."org.test.b5".capabilities]
audio = true
POLICY
for file in services/*; do
    root=$(awk -F'"' '/^path *=[^#]*\/var\/lib\/atrium\/jails/{print $2;exit}' "$file")
    [ -n "$root" ]; install -d "$root/dev"
    awk -F'"' '/^dest *=/{print $2}' "$file" | while read -r dest; do
        case "$dest" in
            var/run/atrium/app-registry) install -d "$root/var/run/atrium"; touch "$root/$dest";;
            *) install -d "$root/$dest";;
        esac
    done
done
kldload -n nullfs
socket=/var/run/atrium/b5-jaild.sock
daemon -o /root/b5-check/jaild.log /usr/local/bin/atrium-jaild serve --policy /root/b5-check/jaild.policy.toml --socket "$socket" --state /root/b5-check/jaild.state
sim=/atrium/sockets/vinculum-controllers/b5-$$.sock
[ ! -e "$sim" ]
daemon -o /root/b5-check/hci.log /root/b5-check/vinculum-hci --controller "tcp:127.0.0.1:$B5_HCI_PORT" --host-listen "$sim" --host-uid 65024
for i in 1 2 3 4 5; do [ -S "$sim" ] && break; sleep 1; done
chgrp _vinculum "$sim"
chmod 0660 "$sim"
daemon -o /root/b5-check/services.log /usr/local/bin/atrium-portcullisd-bootstrap --volumes-socket '' --socket "$socket" --services-dir /root/b5-check/services

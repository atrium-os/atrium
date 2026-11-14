#!/bin/sh
# Samba servers for the share gates (network-shares.md S3), on the Linux
# host, three ports:
#
#   sh scripts/smb-test-server.sh up DIR     # start
#   sh scripts/smb-test-server.sh down DIR   # stop
#
#   445   SMB 3.1.1 only, encryption required: the share to connect to
#   4450  SMB 2.1 at most: refused before any credential
#   4451  SMB 3.1.1 with encryption off: refused before any credential
#
# Share "files" (DIR/files), user smbtest, password in DIR/password. DIR
# holds everything (configs, private dirs, logs). Needs samba
# (apt-get install samba smbclient).
set -eu
cmd=${1:?up or down}
D=${2:?state directory}
mkdir -p "$D"
D=$(cd "$D" && pwd)

down() {
    for n in main old plain; do
        [ -f "$D/$n/run/smbd.pid" ] && kill "$(cat "$D/$n/run/smbd.pid")" 2>/dev/null || true
    done
    pkill -f "smbd.*$D/" 2>/dev/null || true
    sleep 0.5
}

if [ "$cmd" = down ]; then down; exit 0; fi
down

id smbtest >/dev/null 2>&1 || useradd -M -s /usr/sbin/nologin smbtest
[ -f "$D/password" ] || head -c 12 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$D/password"
mkdir -p "$D/files"
chown smbtest "$D/files"
chmod 755 "$D"

conf() { # name port extra
    mkdir -p "$D/$1/run" "$D/$1/lock" "$D/$1/private" "$D/$1/state" "$D/$1/cache" "$D/$1/log"
    cat > "$D/$1/smb.conf" <<CONF
[global]
    server role = standalone server
    smb ports = $2
    pid directory = $D/$1/run
    lock directory = $D/$1/lock
    state directory = $D/$1/state
    cache directory = $D/$1/cache
    private dir = $D/$1/private
    ncalrpc dir = $D/$1/run/ncalrpc
    log file = $D/$1/log/smbd.log
    log level = 1
    passdb backend = tdbsam:$D/private.tdb
    disable netbios = yes
    load printers = no
    printing = bsd
    printcap name = /dev/null
    map to guest = never
    restrict anonymous = 2
    ntlm auth = ntlmv2-only
    $3
[files]
    path = $D/files
    read only = no
    guest ok = no
    valid users = smbtest
    create mask = 0644
    directory mask = 0755
CONF
}
conf main 445 "server min protocol = SMB3_11
    server smb encrypt = required"
conf old 4450 "server max protocol = SMB2_10"
conf plain 4451 "server min protocol = SMB3_11
    server smb encrypt = off"
pw=$(cat "$D/password")
printf '%s\n%s\n' "$pw" "$pw" | smbpasswd -c "$D/main/smb.conf" -s -a smbtest >/dev/null
for n in main old plain; do
    smbd -D -s "$D/$n/smb.conf"
done
i=0
while [ $i -lt 50 ]; do
    [ -f "$D/main/run/smbd.pid" ] && [ -f "$D/old/run/smbd.pid" ] && [ -f "$D/plain/run/smbd.pid" ] && break
    sleep 0.1; i=$((i+1))
done
[ $i -lt 50 ] || { cat "$D"/*/log/smbd.log >&2; exit 1; }
echo "smb: up on 445 (3.1.1, encrypted), 4450 (2.1), 4451 (unencrypted), user smbtest"

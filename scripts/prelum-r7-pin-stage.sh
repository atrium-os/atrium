#!/bin/sh
# Use a fresh synthetic Arca store on the disposable guest's worker socket.
set -eu
P=$1
cd "$P"
test "$(cat .isolated-gate)" = "$2"
test "$(uname -s)" = FreeBSD
socket=/atrium/sockets/arca/arcad.sock
if [ -S "$socket" ]; then
    mv "$socket" "$socket.pre-r7"
fi
umask 077
daemon -f -p "$P/fixture-arca.pid" -o "$P/fixture-arca.log" env \
    ARCA_SOCKET="$socket" ARCA_DIR="$P/fixture-arca" ARCA_APP_REGISTRY="$P/registry" \
    ARCA_VESTIBULUM_UID=0 ARCA_UI_UID="$(id -u nobody)" ARCA_SIGILLUM_SOCKET=none "$P/arcad"
for i in $(seq 20); do
    [ -S "$socket" ] && break
    sleep 1
done
test -S "$socket"
"$P/r7-pin-fixture" unlock
su -m nobody -c "$P/r7-pin-fixture stage $3 $4" >pin-handle
test "$(wc -c <pin-handle | tr -d ' ')" = 16

#!/bin/sh
# The Navigator's pane in Settings, on a live machine (curia.md §15.1):
# who it runs as, what its jail holds, and that its presses change the
# Navigator's list in place. Run inside FreeBSD, as root, with lib/gate.sh
# beside it, while someone uses Settings on the screen:
#
#   sh navigator-settings-pane-check.sh PERSON [PERSONA]
#
# PERSONA is "personal" (the default) or a named one. Before: the Navigator
# bundle installed with its pane (navigator-broker-bundle.sh, fifth
# argument), limend, frescod and Curia running for PERSON, memfed stopped
# (it may move the jail's memory cap, which is compared exactly), and the
# Navigator launched once in PERSONA (so its uid and overlay exist). With
# no list there yet, the script writes two sites into it as the Navigator.
# It then asks, on stdout, for three things in Settings, each waited on for
# at most WAIT seconds (default 180):
#   1. Apps, Navigator, Settings (then PERSONA, if asked).
#   2. Ask again on one site.
#   3. Ask again for every site, then Back to Apps.
set -u
here=$(dirname "$0")
. "$here/lib/gate.sh"
[ "$#" -ge 1 ] || { echo "usage: $0 PERSON [PERSONA]" >&2; exit 2; }
person=$1 persona=${2:-personal}
app=org.atrium.navigator
bundle=/var/lib/atrium/apps/$app
registry=/var/run/atrium/app-registry
wait_s=${WAIT:-180}
gate_begin navigator-settings-pane 19

home=$(pw usershow -n "$person" 2>/dev/null | cut -d: -f9)
[ -n "$home" ] || gate_abort "no user $person"
if [ "$persona" = personal ]; then
    apps=$home/.atrium/apps
    uid=$(awk -v p="$person" -v a="$app" '$2 == p && $3 == a && $0 !~ / persona=/ && $0 !~ / document=/ { u = $1 } END { print u }' "$registry")
else
    apps=$home/.atrium/persona-apps/$persona
    uid=$(awk -v p="$person" -v a="$app" -v s=" persona=$persona" '$2 == p && $3 == a && index($0, s) && $0 !~ / document=/ { u = $1 } END { print u }' "$registry")
fi
[ -n "$uid" ] || gate_abort "the Navigator has no uid for $person in $persona: launch it there once"
overlay=$apps/$app
paned=$apps/.pane-data/$app
data_for=/$(sed -n 's/^data *= *"\(.*\)"$/\1/p' "$bundle/atrium.toml" | head -n 1)
store=$(sed -n 's/^NAVIGATORD_STORE *= *//p' "$bundle/etc/navigatord.conf" | tail -n 1)
[ -n "$store" ] || gate_abort "no NAVIGATORD_STORE in $bundle/etc/navigatord.conf: the list lives in memory"
case "$store" in "$data_for"/*) ;; *) gate_abort "NAVIGATORD_STORE $store is not under the pane data $data_for" ;; esac
[ -x "$bundle/bin/navigator-settings" ] || gate_abort "the bundle has no bin/navigator-settings"
[ -d "$paned" ] || gate_abort "no $paned: launch the Navigator in $persona once with this bundle"
list=$paned${store#"$data_for"}/live-origins
echo "the list: $list (uid $uid)"

if [ ! -s "$list" ]; then
    # As root, then the Navigator's: .pane-data is root's alone.
    d=$(dirname "$list")
    (mkdir -p "$d" && chown -R "$uid:$uid" "$paned" && chmod 700 "$d" \
        && printf 'https://a.example\nhttps://b.example\nhttps://c.example:8443\n' > "$list" \
        && chown "$uid:$uid" "$list" && chmod 600 "$list") || gate_abort "couldn't write the list for uid $uid"
fi
lines() { n=$(grep -c . "$list" 2>/dev/null); echo "${n:-0}"; }
inode=$(stat -f %i "$list")
before=$(lines)
[ "$before" -ge 2 ] || gate_abort "the list has $before sites; it needs two"

# until SECONDS COMMAND...: run COMMAND each second until it succeeds.
until_s() {
    n=$1; shift
    while [ "$n" -gt 0 ]; do
        "$@" && return 0
        sleep 1; n=$((n - 1))
    done
    return 1
}
pane_pid() { pgrep -f '/bin/navigator-settings$' | head -n 1; }
has_pane() { [ -n "$(pane_pid)" ]; }

echo ">>> In Settings: Apps, Navigator, Settings${persona:+ (for $persona, if asked)}"
until_s "$wait_s" has_pane || gate_abort "no navigator-settings process within ${wait_s}s"
pid=$(pane_pid)
jid=$(ps -o jid= -p "$pid" | tr -d ' ')
name=$(jls -j "$jid" name)
path=$(jls -j "$jid" path)
case "$name" in
    app-org-atrium-navigator--*) ok "the pane is in a one-shot jail of the Navigator's: $name" ;;
    *) bad "the pane's jail $name is not a Navigator one-shot jail" ;;
esac
check "it runs as the Navigator's uid in $persona" "$uid" "$(ps -o uid= -p "$pid" | tr -d ' ')"
case "$(procstat -b "$pid" | awk -v p="$pid" '$1 == p { print $NF }')" in
    */bin/navigator-settings) ok "its program is the bundle's pane" ;;
    *) bad "its program is not bin/navigator-settings" ;;
esac
check "no IPv4 or IPv6" "ip4=disable ip6=disable" "$(jls -j "$jid" -n ip4 ip6 | tr -s ' ' | sed 's/^ //;s/ $//')"
check "ATRIUM_PANE_DATA is /app-data" 1 "$(procstat -e "$pid" | tr ' ' '\n' | grep -cx 'ATRIUM_PANE_DATA=/app-data')"
mounts=$(mount -p | awk -v r="$path/" 'index($2, r) == 1 { print $1, $2, $3, $4 }')
echo "$mounts" | sed 's/^/    /'
check "the Navigator's pane data at /app-data, read-write" 1 \
    "$(echo "$mounts" | awk -v s="$paned" -v d="$path/app-data" '$1 == s && $2 == d && $3 == "nullfs" && $4 !~ /(^|,)ro(,|$)/' | grep -c .)"
check "its overlay in no mount of the pane's" 0 "$(echo "$mounts" | awk -v o="$overlay" 'index($1, o) == 1' | grep -c .)"
check "/app-data is noexec" 1 "$(echo "$mounts" | awk -v d="$path/app-data" '$2 == d && $4 ~ /noexec/' | grep -c .)"
check "no service socket in its jail" 0 "$(echo "$mounts" | grep -c '/atrium/sockets\|/var/run')"
check "nothing else of the person's in its jail" 0 "$(echo "$mounts" | awk -v h="$home" -v s="$paned" 'index($1, h) == 1 && $1 != s' | grep -c .)"
check "memoryuse at 256 MiB" 1 "$(rctl "jail:$name" | grep -c ':memoryuse:sigkill=268435456$')"

echo ">>> Press Ask again on one site"
shrunk() { [ "$(lines)" -eq $((before - 1)) ]; }
if until_s "$wait_s" shrunk; then ok "one site went from the list"; else bad "the list has $(lines) sites, not $((before - 1))"; fi
check "the list changed in place" "$inode" "$(stat -f %i "$list")"
check "still the Navigator's, 0600" "$uid 600" "$(stat -f '%u %Lp' "$list")"
check "no temporary or lock file beside it" 1 "$(ls -A "$(dirname "$list")" | grep -c 'live-origins')"

echo ">>> Press Ask again for every site, then Back to Apps"
empty() { [ "$(lines)" -eq 0 ]; }
if until_s "$wait_s" empty; then ok "every site went"; else bad "the list still has $(lines) sites"; fi
check "the list emptied in place" "$inode" "$(stat -f %i "$list")"
gone() { ! has_pane && ! jls -j "$name" >/dev/null 2>&1; }
if until_s "$wait_s" gone; then ok "Back to Apps ended the pane and its jail"; else bad "the pane or its jail $name is still there"; fi
check "its /app-data mount went with it" 0 "$(mount -p | awk -v d="$path/app-data" '$2 == d' | grep -c .)"
gate_end

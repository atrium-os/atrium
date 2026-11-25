#!/bin/sh
# The worker check on a whole live page's engines (navigator-islands.md
# I6.6), the I5 check widened to a page and its frames. Run inside FreeBSD,
# as root, while a fixture page runs live with its cross-origin frame
# (test-assets/navigator-i6, the `frame` line), with lib/gate.sh beside it:
#
#   sh navigator-i6-worker-check.sh PAGE_PID PAGE_MIB CPU_PERCENT [FRAME_PID...]
#
# PAGE_MIB is the installed engine manifest's [resources].memory (1024 for
# the 1G manifest). Every frame engine must have a part's limit, 256 MiB
# (limen::web_island::PART_MEMORY_MB). Stop memfed first: it may tighten
# the engine pool's caps, and the limits are compared exactly. For each
# engine it checks:
#   - its jail is a navigator-engine one-shot jail of its own, with no IP;
#   - it is in Capsicum capability mode and not root;
#   - its jail's RCTL rules: memoryuse sigkill at its role's limit, pcpu;
#   - every executable mapping is read-only, and is the bundle's entry or
#     another file of the bundle, or the kernel's shared page.
# Then that no two engines share a jail, and that each engine is still the
# same process after the checks (no replacement in between).
set -u
here=$(dirname "$0")
. "$here/lib/gate.sh"
[ "$#" -ge 3 ] || { echo "usage: $0 PAGE_PID PAGE_MIB CPU_PERCENT [FRAME_PID...]" >&2; exit 2; }
page=$1 page_mib=$2 cpu=$3
shift 3
part_mib=256
bundle=${NAVIGATOR_ENGINE_BUNDLE:-/var/lib/atrium/apps/org.atrium.navigator.engine}
entry=$(sed -n 's/^entry *= *"\(.*\)"$/\1/p' "$bundle/atrium.toml" | head -n 1)
engines=$((1 + $#))
gate_begin navigator-i6-worker $((engines * 8 + 1))
[ -n "$entry" ] || gate_abort "no entry in $bundle/atrium.toml"

# engine PID MIB ROLE: the checks on one engine.
engine() {
    pid=$1 mib=$2 role=$3
    jid=$(ps -o jid= -p "$pid" 2>/dev/null | tr -d ' ')
    [ -n "$jid" ] && [ "$jid" != 0 ] || { bad "$role $pid: not in a jail"; return; }
    name=$(jls -j "$jid" name)
    case "$name" in
        app-org-atrium-navigator-engine--*) ok "$role $pid: in its engine jail $name" ;;
        *) bad "$role $pid: jail $name is not an engine jail" ;;
    esac
    check "$role $pid: no IPv4 or IPv6" "ip4=disable ip6=disable" \
        "$(jls -j "$jid" -n ip4 ip6 | tr -s ' ' | sed 's/^ //;s/ $//')"
    if procstat -s "$pid" | awk -v p="$pid" '$1 == p && $3 != 0 && $10 ~ /C/ { ok=1 } END { exit !ok }'; then
        ok "$role $pid: Capsicum capability mode, not root"
    else
        bad "$role $pid: not in capability mode, or runs as root"
    fi
    rules=$(rctl "jail:$name")
    check "$role $pid: memoryuse sigkill at $mib MiB" 1 \
        "$(echo "$rules" | grep -Fxc "jail:$name:memoryuse:sigkill=$((mib * 1024 * 1024))")"
    check "$role $pid: pcpu deny at $cpu" 1 "$(echo "$rules" | grep -Fxc "jail:$name:pcpu:deny=$cpu")"
    shared=$(procstat -x "$pid" | awk '$3 == "AT_TIMEKEEP" {print $4}')
    [ -n "$shared" ] && ok "$role $pid: the kernel's shared page is known" || bad "$role $pid: no AT_TIMEKEEP"
    # sh compares hexadecimal addresses without awk's strtonum.
    why=$(procstat -v "$pid" | {
        seen_entry=0 seen_shared=0
        while read -r process start end prot res pres ref shd flags type path; do
            [ "$process" = "$pid" ] || continue
            case "$prot" in *x*) ;; *) continue ;; esac
            case "$prot" in *w*) echo "a writable executable mapping at $start"; exit ;; esac
            case "$type:$path" in
                "vn:$bundle/$entry") seen_entry=1 ;;
                "vn:$bundle/"*) ;;
                ph:) if [ -n "$shared" ] && [ "$((start))" -le "$((shared))" ] && [ "$((shared))" -lt "$((end))" ]; then
                         seen_shared=$((seen_shared + 1))
                     else
                         echo "an anonymous executable mapping at $start"; exit
                     fi ;;
                *) echo "an executable mapping of $type $path"; exit ;;
            esac
        done
        [ "$seen_entry" = 1 ] || { echo "no mapping of $bundle/$entry"; exit; }
        [ "$seen_shared" = 1 ] || { echo "$seen_shared shared-page mappings"; exit; }
    })
    check "$role $pid: executable mappings read-only, the bundle's and the shared page only" "" "$why"
    check "$role $pid: the same process in the same jail after the checks" "$jid" \
        "$(ps -o jid= -p "$pid" 2>/dev/null | tr -d ' ')"
    echo "$jid" >>"$jails"
}

jails=$(mktemp)
trap 'rm -f "$jails"' EXIT
engine "$page" "$page_mib" page
for frame in "$@"; do
    engine "$frame" "$part_mib" frame
done
check "every engine in a jail of its own" "$engines" "$(sort -u "$jails" | grep -c .)"
gate_end

#!/bin/sh
# Merge the repository's jaild policy with the TARGET's [resource_control].
#
#   scripts/jaild-policy-merge.sh <repo-policy> <live-policy|-> > merged.toml
#
# WHY. deploy-portcullis.sh copied etc/jaild.policy.toml over
# /etc/atrium/jaild.policy.toml. The repository's policy has no
# [resource_control] section (its defaults are off, on purpose), so every
# deploy dropped an operator's `allow_rctl = true`, and jaild then REFUSED
# every limited one-shot jail (`rctl.disabled`) until the section was
# re-added by hand (portcullis.md §6.5.2e′, snags). Refusing is the right
# failure; losing the operator's setting is not.
#
# WHAT. The output is the repository's policy, with its own
# [resource_control] section (if it ever gains one) replaced by the live
# file's, when the live file has one. Every key in the live section is kept
# (allow_reap too), not just allow_rctl.
#
#   ALLOW_RCTL=true|false   set allow_rctl explicitly (overrides the live value)
#   ALLOW_REAP=true|false   likewise for allow_reap
#
# With no live section and no override, the repository's policy is output
# unchanged: a fresh machine gets the conservative defaults.
#
# What was decided is reported on stderr, one line.
set -eu
[ $# -eq 2 ] || { echo "usage: $0 <repo-policy> <live-policy|->" >&2; exit 2; }
repo=$1 live=$2
[ -r "$repo" ] || { echo "$0: cannot read $repo" >&2; exit 2; }
[ "$live" = - ] || [ -r "$live" ] || live=-

for v in "${ALLOW_RCTL:-}" "${ALLOW_REAP:-}"; do
    case "$v" in ''|true|false) ;; *) echo "$0: ALLOW_RCTL/ALLOW_REAP must be true or false, not '$v'" >&2; exit 2 ;; esac
done

# The body of one table: the lines after `[name]` up to the next header.
section() {
    awk -v want="$2" '
        /^[ \t]*\[/ { h = $0; sub(/^[ \t]*\[[ \t]*/, "", h); sub(/[ \t]*\].*$/, "", h); on = (h == want); next }
        on && $0 !~ /^[ \t]*(#.*)?$/ { print }' "$1"
}
# The file without that table (header and body).
without() {
    awk -v want="$2" '
        /^[ \t]*\[/ { h = $0; sub(/^[ \t]*\[[ \t]*/, "", h); sub(/[ \t]*\].*$/, "", h); skip = (h == want) }
        !skip { print }' "$1"
}

body=""
from="repository"
if [ "$live" != - ]; then
    lb=$(section "$live" resource_control)
    if [ -n "$lb" ]; then body=$lb; from="target"; fi
fi
[ -n "$body" ] || body=$(section "$repo" resource_control)

# Overrides: replace the key's line if present, else add it.
setkey() {  # setkey <key> <value> — on $body
    if printf '%s\n' "$body" | grep -Eq "^[ \t]*$1[ \t]*="; then
        body=$(printf '%s\n' "$body" | sed -E "s/^[ \t]*$1[ \t]*=.*/$1 = $2/")
    else
        body=$(printf '%s\n%s = %s' "$body" "$1" "$2" | sed '/^$/d')
    fi
    from="$from + $1=$2"
}
[ -z "${ALLOW_RCTL:-}" ] || setkey allow_rctl "$ALLOW_RCTL"
[ -z "${ALLOW_REAP:-}" ] || setkey allow_reap "$ALLOW_REAP"

if [ -z "$body" ]; then
    cat "$repo"
    echo "jaild-policy-merge: no [resource_control] on the target or in the repository; defaults (off)" >&2
    exit 0
fi
without "$repo" resource_control
printf '\n# Kept from the target machine by scripts/jaild-policy-merge.sh (deploy-portcullis.sh):\n'
printf '# the repository policy does not carry the operator'"'"'s resource-control choice.\n'
printf '[resource_control]\n%s\n' "$body"
echo "jaild-policy-merge: [resource_control] from the $from: $(printf '%s' "$body" | tr '\n' ' ')" >&2

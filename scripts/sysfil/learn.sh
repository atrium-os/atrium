#!/bin/sh
# Run a binary in the cloud VM inside a jail whose filter is a jaild class
# in LOG mode, and print what the class is missing (jail-sysfil.md §3.5).
#
#   sh scripts/sysfil/learn.sh <class> <guest-path-of-binary> [args...]
#   MODE=2 sh scripts/sysfil/learn.sh ...   # enforce instead: check a profile
#   JEXEC_USER=navtest ...                  # run as that user (jexec -U)
#
# The binary must already be in the guest. Nothing is built in the VM.
set -eu
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CLASS="$1"; shift
MODE="${MODE:-1}"
PARAMS=$(python3 -I "$ROOT/scripts/sysfil/resolve-profile.py" "$ROOT/etc/jaild.policy.toml" "$CLASS" "$MODE")
J="sysfil-learn-$$"
U=""; [ -n "${JEXEC_USER:-}" ] && U="-U $JEXEC_USER"
"$ROOT/scripts/vssh" "jail -c name=$J path=/ persist $PARAMS >/dev/null &&
  { jexec $U $J $*; echo \"exit status \$?\"; }
  echo \"denied:   \$(jls -j $J -h sysfil.denied | tail -1)\"
  for k in syscalls ioctls sockopts families; do
    echo \"missing \$k: \$(jls -j $J -h sysfil.seen.\$k | tail -1)\"; done
  jail -r $J >/dev/null"

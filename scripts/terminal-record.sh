#!/bin/sh
# Record terminal-program sessions for stoa-term's replay test
# (dev-tools-plan.md M1, scripts/vm-terminal-apps-check.sh).
#
# Each case runs a program in a tmux pane of a fixed size, with
# TERM=xterm-256color, sends it keys, and keeps:
#   <case>.out     every byte the program wrote (tmux pipe-pane -O)
#   <case>.screen  the screen as tmux shows it at the end (the reference)
#   <case>.meta    cols rows cursor_x cursor_y alternate_on
# tmux (ISC) is only the oracle; nothing of it ships. The replay test feeds
# <case>.out into stoa_term::Terminal and compares with the reference.
#
#   sh scripts/terminal-record.sh [outdir] [case ...]
# Needs tmux and the programs (vi = nvi on FreeBSD; vim, nvim, hx, less, top).
set -eu
OUT=${1:-test-assets/terminal}; shift 2>/dev/null || true
mkdir -p "$OUT"
WORK=$(mktemp -d)
trap 'tmux -L rec kill-server 2>/dev/null || true; rm -rf "$WORK"' EXIT

# A file to edit: code, a long line, tabs, a wide character.
cat > "$WORK/sample.rs" <<'RS'
// A sample file for terminal recordings.
fn main() {
	let greeting = "hello, 世界";
	for i in 0..3 {
		println!("{greeting} {i}");
	}
	let long = "this line is long enough to run past the right margin of an eighty column terminal window";
}
RS
seq 1 400 | sed 's/^/line /' > "$WORK/long.txt"

# rec <case> <cols> <rows> <command> [keys | sleep:N] ...
rec() {
    name=$1 cols=$2 rows=$3 cmd=$4; shift 4
    case " ${CASES:-} " in "  "|*" $name "*) ;; *) return 0;; esac
    tmux -L rec kill-server 2>/dev/null || true
    : > "$OUT/$name.out"
    tmux -L rec -f /dev/null new-session -d -s r -x "$cols" -y "$rows" \
        "cd $WORK && sleep 1 && exec env -u TMUX TERM=xterm-256color LANG=C.UTF-8 LC_ALL=C.UTF-8 $cmd"
    tmux -L rec set -g default-terminal xterm-256color
    tmux -L rec set -g escape-time 0
    tmux -L rec pipe-pane -O -t r "cat >> $OUT/$name.out"
    sleep 2.5
    for k in "$@"; do
        case $k in
            sleep:*) sleep "${k#sleep:}" ;;
            lit:*) tmux -L rec send-keys -t r -l "${k#lit:}"; sleep 0.4 ;;
            *) tmux -L rec send-keys -t r "$k"; sleep 0.4 ;;
        esac
    done
    sleep 1.5
    tmux -L rec capture-pane -p -t r > "$OUT/$name.screen"
    tmux -L rec display -p -t r '#{pane_width} #{pane_height} #{cursor_x} #{cursor_y} #{alternate_on}' > "$OUT/$name.meta"
    tmux -L rec kill-server
    echo "recorded $name ($(wc -c < "$OUT/$name.out") bytes)"
}

CASES="$*"
have() { command -v "$1" >/dev/null 2>&1; }

have nvi  && rec nvi-open    80 24 "nvi sample.rs" j j lit:A lit:' // edited' Escape
have vi   && rec vi-open     80 24 "vi sample.rs"  j j j
have vim  && rec vim-open    80 24 "vim -u NONE -N -c 'syntax on' -c 'set number' sample.rs" j j lit:o lit:'new line' Escape
have vim  && rec vim-scroll  80 24 "vim -u NONE -N long.txt" C-f C-f C-b lit:50G
have nvim && rec nvim-open   80 24 "nvim --clean sample.rs" j j lit:A lit:' // x' Escape
have nvim && rec nvim-split  100 30 "nvim --clean -O sample.rs long.txt" C-w l lit:100G
have hx   && rec helix-open  80 24 "hx sample.rs" j j lit:x
have hx   && rec helix-picker 100 30 "hx sample.rs" Space lit:f sleep:1
have less && rec less-long   80 24 "less long.txt" Space Space lit:b lit:/line 3 Enter
have top  && rec top-once    100 30 "top" sleep:2
exit 0

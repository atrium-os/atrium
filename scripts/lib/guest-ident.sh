# guest-ident.sh — sourced by the vm-*.sh harnesses. "Is the guest running OUR code?"
#
# ★★ Ask the running kernel, never a path on disk. Every harness used to hash
# /boot/kernel/tessera_fs.ko. After the 2026-09 upstream resync the kernel boots
# from /boot/atrium-new with its own module directory, so that file was no longer
# the loaded module — and bootstrap's installer puts it in /boot/modules anyway.
# A hash of the wrong file passes the gate while the test measures something else.
# kldstat -v names the file it actually loaded; hash that.
#
# ★ And a module hash alone is not "our code": a boot that fell back to ULE, had
# the RLC controller off, or came up on a non-Tessera root would run a test
# against someone else's scheduler or filesystem and report it as ours.
#
# Both snippets are single-quoted: the guest's /bin/sh expands them, never the host.
#
#   GUEST_KMOD_HASH  prints the loaded tessera_fs.ko's 16-hex sha256 prefix ONLY on
#                    Laminar + ctrl_enable=1 + a tessera root; otherwise it prints
#                    NOT-OUR-STACK[...], which can never equal a KMOD hash, so every
#                    existing `[ "$k" = "$KMOD" ]` gate fails and says why.
#   GUEST_IDENT      one line naming all of it, for log headers.

GUEST_IDENT='ko=$(kldstat -v | sed -n "s/.* tessera_fs.ko (\(.*\))\$/\1/p" | head -1)
  rt=$(mount -p | awk "\$2 == \"/\" {print \$3}")
  echo "ident kmod=$(sha256 -q "$ko" 2>/dev/null | cut -c1-16) sched=$(sysctl -n kern.sched.name) rlc=$(sysctl -n kern.sched.ctrl_enable 2>/dev/null) rootfs=$rt kernel=$(sysctl -n kern.bootfile) module=$ko"'

GUEST_KMOD_HASH='ko=$(kldstat -v | sed -n "s/.* tessera_fs.ko (\(.*\))\$/\1/p" | head -1)
  rt=$(mount -p | awk "\$2 == \"/\" {print \$3}")
  s=$(sysctl -n kern.sched.name); c=$(sysctl -n kern.sched.ctrl_enable 2>/dev/null)
  h=$(sha256 -q "$ko" 2>/dev/null | cut -c1-16)
  if [ "$s" = Laminar ] && [ "$c" = 1 ] && [ "$rt" = tessera ] && [ -n "$h" ]; then echo "$h"
  else echo "NOT-OUR-STACK[sched=$s rlc=$c rootfs=$rt module=${ko:-none}]"; fi'

# ident_ok "<GUEST_IDENT output>" — true only for $KMOD on Laminar/RLC/tessera root.
ident_ok(){ case "$1" in *"ident kmod=$KMOD sched=Laminar rlc=1 rootfs=tessera "*) return 0;; esac; return 1; }

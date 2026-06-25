#!/usr/bin/env python3
"""Injected-loss recovery test for Stoa grid-sync (stoa.md §3.3).

The project's S2 host plan calls for testing "with injected loss/reorder".
This drives a real stoad+stoactl over loopback UDP with stoad dropping
1-in-N OUTPUT datagrams ($STOA_DROP), and asserts a grid-sync client
($STOA_SYNC) still CONVERGES on the correct screen: every marker typed is
present in the final repaint, because the client's seq-gap detection asks
the server to resync whatever loss dropped. A raw byte-stream client has no
such recovery — this is what grid-sync buys on a flaky link.

Usage: build the debug binaries first (`cargo build`), then
    python3 stoa/scripts/loss_recovery_test.py
Exit 0 = all runs converged.
"""
import os, sys, time, pty, select, subprocess, signal, tempfile, fcntl, termios, struct

HERE = os.path.dirname(os.path.abspath(__file__))
BIN = os.environ.get("STOA_BIN", os.path.join(HERE, "..", "target", "debug"))
PREFIX = b"\x02"  # Ctrl-B


def drain(fd, secs=0.8):
    buf = b""
    end = time.time() + secs
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try:
                d = os.read(fd, 65536)
            except OSError:
                break
            if not d:
                break
            buf += d
    return buf


def run(drop_n=3):
    ctl = tempfile.mktemp(suffix=".ctl")
    base = dict(os.environ, STOA_CTL=ctl, STOA_DROP=str(drop_n))
    stoad = subprocess.Popen([os.path.join(BIN, "stoad")], env=base,
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(0.7)
    pid, fd = pty.fork()
    if pid == 0:
        env = dict(base, TERM="xterm", STOA_SYNC="1")
        os.environ.update(env)
        fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
        os.execvpe(os.path.join(BIN, "stoactl"),
                   [os.path.join(BIN, "stoactl"), "attach", "losstest"], os.environ)
    time.sleep(0.9)
    drain(fd)
    os.write(fd, b"echo LOSSMARK_AAA\n")
    time.sleep(0.4)
    # Trailing output drives resync past any drops (deterministic 1-in-N means
    # a non-dropped post-change datagram always arrives to trigger recovery).
    for i in range(8):
        os.write(fd, b"echo settle%d\n" % i)
        time.sleep(0.15)
    final = drain(fd, 1.5)
    os.write(fd, PREFIX + b"d")
    time.sleep(0.3)
    try:
        os.close(fd)
    except OSError:
        pass
    os.waitpid(pid, 0)
    stoad.send_signal(signal.SIGTERM)
    stoad.wait()
    return final


def main():
    markers = [b"LOSSMARK_AAA"] + [b"settle%d" % i for i in range(8)]
    allok = True
    for r in range(3):
        out = run()
        present = [m for m in markers if m in out]
        ok = len(present) == len(markers)
        allok = allok and ok
        print(f"run {r+1}: sync under 1/3 loss converged "
              f"({len(present)}/{len(markers)} markers): {ok}")
    print("\nRESULT:", "PASS" if allok else "FAIL")
    sys.exit(0 if allok else 1)


if __name__ == "__main__":
    main()

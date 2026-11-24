#!/usr/bin/env python3
"""Open any page in the VPN sign-in window (navigator-islands.md "I6.0
qualification run"), on a guest staged by crypta-sign-in-gate.py's stage().

The window is the one built path that runs a whole page live. To open a page
that no gateway names, portcullisd talks to a stand-in for cryptad's socket
(navigator-i6-window/fake-crypta.c: every `sso-attempt` gets one live
attempt; it reads a whole line, because portcullisd writes the request in
more than one write).

The fixture names (navigator-i6-fixture.py) reach the host through one QEMU
forward: boot with
    -netdev user,...,guestfwd=tcp:10.0.2.102:443-cmd:/usr/bin/nc 127.0.0.1 PORT
(a script that execs nc, as the Crypta relays do) and serve the fixture on
PORT:
    python3 scripts/navigator-i6-fixture.py cert DIR
    python3 scripts/navigator-i6-fixture.py serve DIR --port PORT > fixture.log

  navigator-i6-window.py setup --ssh VSSH --ca FIXTURE_CA [--gate-ca GATE_CA]
      names -> 10.0.2.102 in /etc/hosts, the couriers trust FIXTURE_CA (and
      GATE_CA), resolv.conf has a server, the stand-in runs and portcullisd
      uses it
  navigator-i6-window.py open --ssh VSSH --url URL --evidence DIR [--settle S]
      asks portcullisd for the window, waits, and keeps the screen and the
      window's log lines
"""
import argparse
import json
import subprocess
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
NAMES = ("app.example", "api.app.example", "pay.example", "cdn.thirdparty.example", "elsewhere.example")
FAKE = "/tmp/i6-crypta.sock"
PORTCULLIS = "/atrium/sockets/portcullis/portcullis.sock"


def run(ssh, command, data=None, timeout=60):
    r = subprocess.run([str(ssh), command], input=data, capture_output=True, timeout=timeout)
    if r.returncode:
        raise SystemExit(f"guest command failed ({r.returncode}): {command}\n{(r.stdout + r.stderr).decode(errors='replace')}")
    return r.stdout.decode(errors="replace")


def setup(a):
    ssh = a.ssh.resolve()
    hosts = "".join(f"10.0.2.102 {n}\n" for n in NAMES)
    run(ssh, "sed -i '' '/ app.example$/d;/\\.example$/d' /etc/hosts; cat >> /etc/hosts", hosts.encode())
    bundle = Path(a.ca).read_bytes() + (Path(a.gate_ca).read_bytes() if a.gate_ca else b"")
    run(ssh, "cat > /tmp/i6-ca.pem", bundle)
    # The guest has no headers: cross-build the stand-in here (RUNBOOK §4).
    root = HERE.parent
    out = Path(a.build_dir) / "i6-fake-crypta"
    out.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run([a.cc, "--target=aarch64-unknown-freebsd", f"--sysroot={root / 'sysroot'}", "-fuse-ld=lld", "-O2",
                    "-o", str(out), str(HERE / "navigator-i6-window" / "fake-crypta.c")], check=True)
    run(ssh, "cat > /tmp/i6-fake-crypta && chmod 755 /tmp/i6-fake-crypta", out.read_bytes())
    run(ssh, f"""set -e
grep -q '^nameserver' /etc/resolv.conf || echo 'nameserver 10.0.2.3' >> /etc/resolv.conf
pkill -f /tmp/i6-fake-crypta || true
daemon -f -o /tmp/i6-fake-crypta.log /tmp/i6-fake-crypta {FAKE}
for i in $(jot 20); do test -S {FAKE} && break; sleep .25; done; test -S {FAKE}
S=/atrium/sockets
export ARCA_WEB_BROKER="org.atrium.navigator $(openssl pkey -in /tmp/mail-button/gate-key.pem -pubout -outform DER | sha256 -q)"
pkill -f arca-courierd || true; sleep 1; [ ! -S $S/arca-courier/courierd.sock ] || unlink $S/arca-courier/courierd.sock
env ARCA_COURIERD_CA=/tmp/i6-ca.pem ARCA_COURIERD_GRANT_PRIVATE_NETWORK=1 daemon -f -o /tmp/i3-vpn/courier.log /tmp/mail-button/arca-courierd
p=$(pgrep -x portcullisd); kill $p; for i in $(jot 30); do kill -0 $p 2>/dev/null || break; sleep .2; done
[ ! -S {PORTCULLIS} ] || unlink {PORTCULLIS}
env PORTCULLISD_CRYPTA_SOCK={FAKE} PORTCULLISD_FRESCO_ACTIVATION=$S/fresco-activation/fresco-activation.sock daemon -f -o /tmp/i3-portcullisd.log /tmp/i3-vpn/portcullisd --socket {PORTCULLIS}
for i in $(jot 50); do test -S {PORTCULLIS} && test -S $S/arca-courier/courierd.sock && break; sleep .2; done
test -S {PORTCULLIS}; test -S $S/arca-courier/courierd.sock
su -m mailgate -c '/tmp/mail-button/portcullis daemon reload' >/dev/null
grep '^nameserver' /etc/resolv.conf
""")
    print("navigator-i6-window: set up")


def open_page(a):
    ssh = a.ssh.resolve()
    ev = Path(a.evidence)
    ev.mkdir(parents=True, exist_ok=False)
    run(ssh, "j=$(jls -j org_atrium_navigator__p_vpn jid 2>/dev/null) && pkill -j $j -x navigatord; sleep 2; true")
    host = a.url.split("/")[2]
    final = f"https://{host}/atrium-i6-never-reached"
    ask = json.dumps({"op": "hello", "version": 1}) + "\n" + json.dumps(
        {"op": "vpn_sign_in_window", "profile": "win", "start_url": a.url, "final_url": final, "activation": None}) + "\n"
    run(ssh, "cat > /tmp/i6-ask && chmod 644 /tmp/i6-ask", ask.encode())
    offsets = run(ssh, "wc -c < /tmp/i3-portcullisd.log; wc -c < /tmp/i3-vpn/limen.log").split()
    reply = run(ssh, f"su -m mailgate -c 'nc -w 5 -N -U {PORTCULLIS} < /tmp/i6-ask'")
    (ev / "ask.txt").write_text(reply)
    time.sleep(a.settle)
    (ev / "screen.png").write_bytes(subprocess.run([str(ssh), "cat /tmp/i3-vpn/screen.png"], capture_output=True).stdout)
    (ev / "portcullisd.log").write_text(run(ssh, f"tail -c +{int(offsets[0]) + 1} /tmp/i3-portcullisd.log"))
    (ev / "worker.log").write_text(run(ssh, f"tail -c +{int(offsets[1]) + 1} /tmp/i3-vpn/limen.log"))
    print(f"navigator-i6-window: {a.url} -> {ev}")


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("setup")
    s.add_argument("--ssh", type=Path, required=True)
    s.add_argument("--ca", required=True)
    s.add_argument("--gate-ca")
    s.add_argument("--cc", default="/opt/homebrew/opt/llvm/bin/clang")
    s.add_argument("--build-dir", default=str(Path.home() / ".cache" / "atrium-i6-window"))
    o = sub.add_parser("open")
    o.add_argument("--ssh", type=Path, required=True)
    o.add_argument("--url", required=True)
    o.add_argument("--evidence", required=True)
    o.add_argument("--settle", type=float, default=30)
    a = p.parse_args()
    setup(a) if a.cmd == "setup" else open_page(a)


if __name__ == "__main__":
    main()

#!/usr/bin/env python3
"""Exercise the installed AnyConnect Sign In window in a disposable Tessera guest."""

import argparse
import json
import re
import subprocess
import sys
import time
from contextlib import contextmanager
from pathlib import Path


SUPPORT = Path(__file__).resolve().with_name("crypta-sign-in-gate")
GUEST = "/tmp/i3-vpn"
CTL = "/tmp/i3-net/ctl"
LOG = "/tmp/i3-portcullisd.log"
JAIL = "org_atrium_navigator__p_vpn"
HOLD = "/tmp/i3-hold-vpn-queue-worker"
SOCKET = "/atrium/sockets/portcullis/portcullis.sock"
SENTINEL = "http://10.98.0.1/"
TRAFFIC_FIELDS = ("script_posts", "script_accepted", "acs_ok", "token_ok", "proxied")
REQUEST = (
    {"op": "hello", "version": 1},
    {
        "op": "vpn_sign_in_window",
        "profile": "win",
        "start_url": "https://sso.crypta.test:447/+CSCOE+/saml/sp/login",
        "final_url": "https://sso.crypta.test:447/+CSCOE+/saml_ac_login.html",
        "activation": None,
    },
)


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def until(description, predicate, timeout=20, interval=0.25):
    deadline = time.monotonic() + timeout
    while True:
        value = predicate()
        if value:
            return value
        remaining = deadline - time.monotonic()
        require(remaining > 0, f"Timed out waiting for {description}")
        time.sleep(min(interval, remaining))


class Gate:
    def __init__(self, args):
        self.args = args
        self.attempts = set()
        self.evidence = args.evidence.resolve()
        self.evidence.mkdir(mode=0o700)

    def remote(self, command, *, data=None, source=None, check=True, timeout=35):
        result = subprocess.run(
            [str(self.args.ssh), command], input=data, stdin=source,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout,
        )
        if check and result.returncode:
            detail = (result.stdout + result.stderr).decode(errors="replace").strip()
            raise RuntimeError(f"Guest command failed ({result.returncode}): {detail}")
        return result

    def guest(self, command):
        return self.remote(command).stdout.decode().strip()

    def stage(self):
        files = self.args.fixtures.resolve()
        bins = ("arca-ui", "limend", "portcullisd", "navigatord", "navigator-ui", "mail-page")
        for name in (*bins, "ui-bundles.tgz", "net.tgz", "atrium-cryptad"):
            require((files / name).stat().st_size > 0, f"Missing fixture input {name}")
        renderer = self.args.renderer
        require(re.fullmatch(r"/[a-zA-Z0-9_./-]+", renderer), "Use an absolute guest renderer path")
        self.guest(
            'test "$(uname -m)" = arm64 && '
            'test "$(mount -p | awk \'$2 == "/" {print $3}\')" = tessera && '
            "test -x /tmp/mail-button/arcad && test -f /tmp/mail-button/gate-key.pem && "
            "test ! -e /tmp/i3-vpn && test ! -e /tmp/i3-net && "
            f'test -x {renderer} && mkdir -p {GUEST} /tmp/i3-net'
        )
        with (self.evidence / "stage.log").open("wb") as log:
            def stage_file(command, path):
                with path.open("rb") as source:
                    result = self.remote(command, source=source, check=False, timeout=90)
                log.write(result.stdout + result.stderr)
                log.flush()
                require(result.returncode == 0, f"Staging {path.name} failed. See {log.name}")

            stage_file(f"cd {GUEST} && tar --no-xattrs -xzf -", files / "ui-bundles.tgz")
            for name in bins:
                stage_file(f"cat > {GUEST}/{name} && chmod 755 {GUEST}/{name}", files / name)
            for name in ("navigator-broker-bundle.sh", "navigator-page-bundle.sh", "navigator-ui-bundle.sh"):
                stage_file(f"cat > {GUEST}/{name}", SUPPORT.parent / name)
            stage_file(f"cat > {GUEST}/ca.pem", self.args.ca)
            stage_file(f"sh -s -- {renderer}", SUPPORT / "services.sh")
            stage_file("cd /tmp/i3-net && tar --no-xattrs -xzf -", files / "net.tgz")
            stage_file("cat > /tmp/i3-net/atrium-cryptad", files / "atrium-cryptad")
            stage_file("sh -s", SUPPORT / "network.sh")

        until("egress interface membership",
              lambda: "atrium_egress" in self.guest("ifconfig vtnet0"))
        wire = "".join(json.dumps(frame) + "\n" for frame in REQUEST).encode()
        self.remote("cat > /tmp/i3-ask && chmod 644 /tmp/i3-ask", data=wire)

    def stats(self, label=None):
        port = self.args.gateway_port
        payload = subprocess.check_output(
            ["curl", "--noproxy", "*", "-fsS", "--max-time", "5",
             "--cacert", str(self.args.ca), "--resolve", f"sso.crypta.test:{port}:127.0.0.1",
             f"https://sso.crypta.test:{port}/stats"],
            timeout=7,
        )
        stats = json.loads(payload)
        if label:
            (self.evidence / f"{label}.json").write_text(json.dumps(stats, sort_keys=True) + "\n")
        return stats

    def counters(self):
        log = self.guest(f"cat {LOG}")
        (self.evidence / "portcullisd.log").write_text(log + "\n")
        return {
            "queued": log.count("portcullisd: queued VPN sign-in window for mailgate (win)"),
            "replaced": log.count("portcullisd: replaced queued VPN sign-in window for mailgate (win)"),
            "discarded": log.count("portcullisd: discarded queued VPN sign-in for win"),
            "launched": log.count("for mailgate (a queued VPN sign-in window)"),
            "held": log.count("fixture: VPN queue worker held"),
            "hold_expired": log.count("fixture: VPN queue worker hold expired"),
            "reloads": log.count("a key from the page: Reload"),
        }

    def ask(self):
        reply = self.guest(f"su -m mailgate -c 'nc -w 5 -N -U {SOCKET} < /tmp/i3-ask'")
        responses = [json.loads(line) for line in reply.splitlines()]
        require(responses and responses[-1] == {"kind": "ok"}, f"Sign In failed: {responses}")

    def attempt(self):
        response = self.remote(f"{CTL} sso-attempt name=win", check=False)
        if response.returncode:
            require(response.returncode != 255, "SSH failed while checking the Crypta attempt")
            return None
        match = re.search(rb"^sso-attempt id=([0-9a-f]{32})$", response.stdout, re.MULTILINE)
        require(match, "Missing Crypta attempt identifier")
        return match.group(1)

    def new_attempt(self):
        attempt = until("Crypta sign-in attempt", self.attempt, timeout=30)
        require(attempt not in self.attempts, "Crypta reused a previous sign-in attempt")
        self.attempts.add(attempt)
        return attempt

    def up(self):
        self.guest(f"{CTL} up name=win")
        return self.new_attempt()

    def down(self):
        self.guest(f"{CTL} down name=win")
        require(self.attempt() is None, "Disconnect did not invalidate the attempt")

    def jid(self):
        result = self.remote(f"jls -j {JAIL} jid", check=False)
        if result.returncode:
            require(result.returncode != 255, "SSH failed while checking the Navigator jail")
            return None
        return int(result.stdout)

    @contextmanager
    def paused_window(self):
        jid = until("Navigator sign-in jail", self.jid)
        pid = int(self.guest(f"pgrep -j {jid} -x navigatord"))
        self.guest(f"kill -STOP {pid}")
        try:
            until("broker stopped", lambda: self.guest(f"ps -o state= -p {pid}").startswith("T"))
            yield jid
        finally:
            self.guest(f"kill -CONT {pid}")

    @contextmanager
    def held_worker(self):
        self.guest(f"test ! -e {HOLD} && touch {HOLD}")
        try:
            yield
        finally:
            self.guest(f"rm -f {HOLD}")

    def input(self, *lines):
        self.remote(f"nc -w 3 -N -U {GUEST}/input.sock", data=("\n".join(lines) + "\n").encode())

    def click(self, x, y):
        self.input(f"MOVE {x} {y}", "BTN 1 1", "BTN 1 0")

    def type_fixture_text(self, value):
        lines = []
        for char in value:
            hid = 45 if char == "-" else ord(char) - ord("a") + 4
            lines.extend([f"KEY {hid} 1 0", f"KEY {hid} 0 0"])
        self.input(*lines)

    def screen(self, label):
        path = self.evidence / f"{label}.png"
        path.write_bytes(self.remote(f"cat {GUEST}/screen.png").stdout)
        return subprocess.check_output(
            [self.args.ocr, str(path), "stdout", "--psm", "11"],
            stderr=subprocess.DEVNULL, text=True, timeout=8,
        )

    def show(self, label, pattern):
        return until(label, lambda: re.search(pattern, self.screen(label), re.IGNORECASE), interval=0.5)

    def page(self, before, label):
        def ready():
            after = self.stats()
            require(after["sso_login"] <= before["sso_login"] + 1, "More than one Sign In page opened")
            return after["sso_login"] == before["sso_login"] + 1 and after["script_delivered"] > before["script_delivered"]

        until(label, ready, timeout=24)
        self.show(label, r"vpn sign.in")
        require(not re.search(r"sign.in code", self.screen(label), re.IGNORECASE),
                "A fresh page inherited a stale proposal")

    def pending(self, label):
        self.click(320, 97)
        self.show(label, r"sign.in code")
        return self.no_traffic(label)

    def no_traffic(self, label):
        stats = self.stats(label)
        require(all(stats[key] == 0 for key in TRAFFIC_FIELDS), f"Unexpected submission during {label}")
        status = self.guest(f"{CTL} list")
        require(re.search(r"name=win.*handshake=0", status), f"Unexpected handshake during {label}")
        self.no_ipv4()
        return stats

    def no_ipv4(self):
        status = self.guest(
            f"if fetch -T 2 -qo - {SENTINEL} >/dev/null 2>&1; "
            "then echo reachable; else echo blocked; fi"
        )
        require(status == "blocked", "IPv4 sentinel was reachable before confirmation")

    def queued_window(self, old, attempt, prior_launches):
        until("queued sign-in window", lambda: (jid := self.jid()) and jid != old)
        require(self.attempt() == attempt, "The new window did not use the newest attempt")
        require(self.counters()["launched"] == prior_launches + 1, "Expected exactly one queued launch")

    def cancel_queued(self, old, baseline):
        require(self.counters()["queued"] == baseline["queued"] + 1 and self.jid() == old,
                "Reconnect did not queue before teardown")
        self.down()
        until("discard after disconnect",
              lambda: self.counters()["discarded"] == baseline["discarded"] + 1, timeout=6)
        require(self.counters()["launched"] == baseline["launched"] and self.jid() == old,
                "Cancelled request opened a window or the old window closed early")
        self.no_ipv4()
        require(all(self.stats()[key] == 0 for key in TRAFFIC_FIELDS), "Cancelled request submitted data")

    def run(self):
        initial = self.stats("initial")
        require(initial["init"] == initial["sso_login"] == 0, "Use a fresh gateway fixture")
        require(all(initial[key] == 0 for key in TRAFFIC_FIELDS), "Gateway already recorded a submission")
        self.stage()

        self.new_attempt()
        self.ask()
        self.page(initial, "initial-page")
        for number in (1, 2):
            before = self.pending(f"before-reload-{number}")
            self.input("KEY 21 1 1", "KEY 21 0 1")
            self.page(before, f"reload-{number}")
            require(self.counters()["reloads"] == number, "Reload did not reach the page")
            self.no_traffic(f"after-reload-{number}")
        before = self.pending("before-replacement")
        print("PASS reload discards a pending proposal twice", flush=True)

        baseline = self.counters()
        with self.paused_window() as old:
            self.down()
            self.up()
            with self.held_worker():
                self.ask()
                until("fixture worker barrier", lambda: self.counters()["held"] == baseline["held"] + 1)
                queued = self.counters()
                require(queued["queued"] == baseline["queued"] + 1, "Sign In did not queue")
                self.ask()
                self.ask()
                require(self.counters() == queued, "Duplicate Sign In altered the queue")
                print("PASS duplicate Sign In keeps one queued request", flush=True)
                for replacement in (1, 2):
                    self.down()
                    latest = self.up()
                    self.ask()
                    counts = self.counters()
                    require(counts["replaced"] == baseline["replaced"] + replacement
                            and counts["queued"] == queued["queued"]
                            and counts["launched"] == baseline["launched"]
                            and counts["discarded"] == baseline["discarded"]
                            and counts["hold_expired"] == baseline["hold_expired"]
                            and self.jid() == old, "Replacement was not in place")
                self.click(561, 131)
        self.queued_window(old, latest, baseline["launched"])
        self.page(before, "after-replacement")
        before = self.pending("replacement-pending")
        print("PASS repeated replacement opens only the newest attempt after teardown", flush=True)

        baseline = self.counters()
        with self.paused_window() as old:
            self.down()
            self.up()
            self.ask()
            self.cancel_queued(old, baseline)
            latest = self.up()
            self.ask()
            require(self.counters()["queued"] == baseline["queued"] + 2 and self.jid() == old,
                    "Sign In did not requeue before teardown")
            self.click(561, 131)
        self.queued_window(old, latest, baseline["launched"])
        self.page(before, "after-cancelled-requeue")
        before = self.pending("requeue-pending")
        print("PASS requeue after cancellation launches once with a fresh proposal", flush=True)

        baseline = self.counters()
        with self.paused_window() as old:
            self.down()
            self.up()
            self.ask()
            self.cancel_queued(old, baseline)
            self.click(561, 131)
            print("PASS disconnect cancels Sign In while the previous window is open", flush=True)

            live = self.up()
            started = time.monotonic()
            self.ask()
            counts = self.counters()
            require(counts["queued"] == baseline["queued"] + 2, "Expiry test did not queue Sign In")
            time.sleep(6)
            require(self.attempt() == live and self.jid() == old and self.counters() == counts,
                    "Queue changed before the expiry probe")
            self.ask()
            require(self.counters() == counts, "Duplicate Sign In changed the expiry request")
            until("queue expiry for a live attempt",
                  lambda: self.counters()["discarded"] == baseline["discarded"] + 2, timeout=6)
            require(9 <= time.monotonic() - started < 14, "Queue expiry did not follow its original deadline")
            require(self.attempt() == live and self.jid() == old, "Expiry was caused by teardown or disconnect")
        until("old window teardown", lambda: self.jid() is None)
        require(self.counters()["launched"] == baseline["launched"], "Expired request opened a window")
        expired = self.no_traffic("after-expiry")
        require(all(expired[k] == before[k] for k in ("sso_login", "script_delivered")),
                "A cancelled or expired request fetched a sign-in page")
        print("PASS live request expires at its original deadline despite duplicate Sign In", flush=True)

        self.down()
        self.up()
        self.ask()
        self.page(expired, "after-teardown-reconnect")
        self.pending("fresh-confirmation")
        self.click(561, 131)
        self.show("idp", r"username or email")
        self.click(587, 95)
        self.type_fixture_text("alice")
        self.click(588, 166)
        self.type_fixture_text("alice-sso-pw")
        self.click(560, 131)
        self.show("assertion", r"Authentication Redirect")
        self.click(557, 95)

        def tunnel():
            status = self.guest(f"{CTL} list")
            return re.search(r"name=win.*iface=(tun[0-9]+).*handshake=1.*starts=1", status)

        iface = until("ocserv handshake", tunnel, timeout=30)[1]
        route = self.guest("route -n get 10.98.0.1")
        require(re.search(rf"interface:\s+{iface}\b", route), f"Sentinel route did not use {iface}")
        require(self.guest(f"fetch -T 10 -qo - {SENTINEL}") == "crypta-ac-ok", "Unexpected sentinel body")
        final = self.stats("final")
        require(all(final[key] == 1 for key in TRAFFIC_FIELDS), f"Expected one confirmed submission: {final}")
        self.counters()
        print(f"PASS fresh confirmation sends one POST and fetches crypta-ac-ok via {iface}", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ssh", required=True, type=Path, help="root SSH wrapper for the dedicated arm64 guest")
    parser.add_argument("--fixtures", required=True, type=Path, help="directory of host-built fixture artifacts")
    parser.add_argument("--ca", required=True, type=Path, help="CA used by the local gateway and IdP")
    parser.add_argument("--renderer", required=True, help="guest-local signed Fresco renderer bundle")
    parser.add_argument("--evidence", required=True, type=Path, help="new directory for counters, screenshots and trace")
    parser.add_argument("--gateway-port", type=int, default=8447)
    parser.add_argument("--ocr", default="tesseract")
    args = parser.parse_args()
    args.ssh = args.ssh.resolve()
    args.ca = args.ca.resolve()
    try:
        gate = Gate(args)
        gate.run()
    except (OSError, RuntimeError, ValueError, subprocess.SubprocessError) as error:
        print(f"FAIL: {error}. Evidence: {args.evidence}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

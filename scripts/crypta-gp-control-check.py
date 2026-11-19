#!/usr/bin/env python3
"""Run the Crypta host harness against OpenConnect's fake-gp-server.py.

Build the harness and engine with:
  cargo build --manifest-path crypta/Cargo.toml -p crypta-engine \
    --bin atrium-crypta-ac --example ac_harness

Run this script with a Python environment containing Flask. Supply the path
to tests/fake-gp-server.py from the OpenConnect 9.21 release. The final HTTP
502 is the fixture's intentional TLS tunnel rejection.
"""

import os
from pathlib import Path
import re
import signal
import socket
import ssl
import subprocess
import sys
import tempfile
import time
from urllib.request import Request, urlopen


def main():
    if len(sys.argv) != 2:
        raise SystemExit(f"usage: {sys.argv[0]} /path/to/fake-gp-server.py")
    fixture = Path(sys.argv[1]).resolve(strict=True)
    target = Path(__file__).resolve().parents[1] / "crypta/target/debug"
    with tempfile.TemporaryDirectory() as directory:
        tmp = Path(directory)
        config = tmp / "openssl.cnf"
        config.write_text(
            "[req]\ndistinguished_name=dn\nx509_extensions=v3\n"
            "[dn]\n[v3]\nsubjectAltName=DNS:localhost\n"
        )
        cert, key = tmp / "server.pem", tmp / "key.pem"
        subprocess.run([
            "openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
            "-days", "1", "-config", config, "-subj", "/CN=localhost",
            "-keyout", key, "-out", cert,
        ], check=True, capture_output=True)
        fingerprint = subprocess.check_output([
            "openssl", "x509", "-in", cert, "-noout", "-fingerprint", "-sha256",
        ], text=True)
        pin = fingerprint.strip().split("=", 1)[1].replace(":", "")
        # The upstream development server requires a fixed port argument.
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        with (tmp / "server.log").open("w+") as log:
            server = subprocess.Popen(
                [sys.executable, fixture, "127.0.0.1", str(port), cert, key],
                stdout=log, stderr=log, start_new_session=True,
            )
            try:
                for _ in range(100):
                    if server.poll() is not None:
                        raise SystemExit("OpenConnect fixture exited during startup")
                    try:
                        with socket.create_connection(("127.0.0.1", port), timeout=0.1):
                            break
                    except OSError:
                        time.sleep(0.1)
                else:
                    raise SystemExit("OpenConnect fixture never opened its TLS port")
                # Test TLS only. The fixture's optional ESP XML in 9.21
                # contains a mismatched closing tag.
                setup = Request(f"https://localhost:{port}/CONFIGURE", data=b"esp=0")
                with urlopen(setup, context=ssl.create_default_context(cafile=cert), timeout=5) as response:
                    if response.status != 201:
                        raise SystemExit("OpenConnect fixture configuration failed")
                result = subprocess.run([
                    target / "examples/ac_harness", target / "atrium-crypta-ac",
                    "localhost", f"127.0.0.1:{port}", cert, "--gp", "--pin", pin,
                    "username=fixture", "password=fixture-only",
                ], capture_output=True, text=True, timeout=30, check=True)
                # Report method, path and status only. The fixture's URL
                # and debug output can contain synthetic session tokens.
                log.seek(0)
                log_text = re.sub(r"\x1b\[[0-9;]*m", "", log.read())
                events = re.findall(r'"(POST|GET) (/[^? "\r\n]+)(?:\?[^ "\r\n]*)? HTTP/1\.[01]"\s+(\d{3})', log_text)
                for method, path, status in events:
                    print(method, path, status)
                refusal = next((line for line in result.stdout.splitlines() if line.startswith("refused:")), "")
                print(refusal)
                expected = [
                    ("POST", "/CONFIGURE", "201"),
                    ("POST", "/ssl-vpn/prelogin.esp", "200"),
                    ("POST", "/ssl-vpn/login.esp", "200"),
                    ("POST", "/ssl-vpn/getconfig.esp", "200"),
                    ("POST", "/ssl-vpn/hipreportcheck.esp", "200"),
                    ("GET", "/ssl-tunnel-connect.sslvpn", "502"),
                ]
                if events != expected or refusal != "refused: GlobalProtect TLS tunnel refused":
                    raise SystemExit("GlobalProtect control-plane check failed")
                print("GlobalProtect control-plane check passed")
            finally:
                try:
                    os.killpg(server.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                server.wait(timeout=5)


if __name__ == "__main__":
    main()

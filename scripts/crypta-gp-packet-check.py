#!/usr/bin/env python3
"""GlobalProtect TLS packet peer for the Crypta host and FreeBSD checks."""

import argparse
import os
from pathlib import Path
import shlex
import socket
import ssl
import struct
import subprocess
import sys
import tempfile
import threading
import time
from urllib.parse import parse_qs, urlsplit

ROOT = Path(__file__).resolve().parents[1]
LOCAL = socket.inet_aton("10.77.0.2")
REMOTE = socket.inet_aton("10.77.0.1")
MAGIC = bytes.fromhex("1a2b3c4d")
COOKIE = "0123456789abcdef0123456789abcdef"
PASSWORD = "a&+ b"


def checksum(data):
    data = bytes(data)
    if len(data) % 2:
        data += b"\0"
    total = sum(struct.unpack(f"!{len(data) // 2}H", data))
    while total >> 16:
        total = (total & 0xffff) + (total >> 16)
    return total ^ 0xffff


def echo_reply(packet):
    packet = bytearray(packet)
    offset = (packet[0] & 15) * 4
    assert packet[12:20] == LOCAL + REMOTE and packet[9] == 1
    assert packet[offset] == 8 and checksum(packet[offset:]) == checksum(packet[:offset]) == 0
    packet[12:20] = REMOTE + LOCAL
    packet[offset] = 0
    packet[offset + 2:offset + 4] = b"\0\0"
    packet[offset + 2:offset + 4] = struct.pack("!H", checksum(packet[offset:]))
    packet[10:12] = b"\0\0"
    packet[10:12] = struct.pack("!H", checksum(packet[:offset]))
    return packet


def frames(stream, reader, calls):
    stream.sendall(b"START_TUNNEL")
    while header := reader.read(16):
        assert len(header) == 16 and header[:4] == MAGIC
        length = int.from_bytes(header[6:8], "big")
        if not length:
            stream.sendall(header)
            continue
        assert header[4:6] == b"\x08\x00" and 28 <= length <= 1500
        packet = reader.read(length)
        assert len(packet) == length
        reply = echo_reply(packet)
        stream.sendall(header + reply)
        calls.append("echo")


def serve(ctx, listener, calls, errors):
    try:
        while True:
            raw, _ = listener.accept()
            with ctx.wrap_socket(raw, server_side=True) as stream:
                stream.settimeout(15)
                with stream.makefile("rb") as reader:
                    first = reader.readline().decode().split()
                    if not first:
                        continue
                    _, url, _ = first
                    parsed = urlsplit(url)
                    path = parsed.path.rsplit("/", 1)[-1]
                    calls.append(path)
                    headers = {}
                    while (line := reader.readline()) != b"\r\n":
                        name, value = line.decode().split(":", 1)
                        headers[name.lower()] = value.strip()
                    fields = parse_qs(reader.read(int(headers.get("content-length", "0"))).decode())
                    query = parse_qs(parsed.query)
                    if path == "prelogin.esp":
                        assert (query or fields)["clientos"] == ["Linux"]
                        body = ("<prelogin-response><status>Success</status>"
                                "<username-label>Username</username-label>"
                                "<password-label>Password</password-label></prelogin-response>")
                    elif path == "login.esp":
                        assert fields["user"] == ["alice"] and fields["passwd"] == [PASSWORD]
                        args = ["", COOKIE, "fixture", "office", "alice", "ldap", "vsys", "corp",
                                "", "", "", "", "tunnel", "-1", "4100", "10.77.0.2"]
                        body = ("<jnlp><application-desc>"
                                + "".join(f"<argument>{a}</argument>" for a in args)
                                + "</application-desc></jnlp>")
                    elif path == "getconfig.esp":
                        assert fields["authcookie"] == [COOKIE] and fields["portal"] == ["office"]
                        body = ('<response status="success"><ssl-tunnel-url>/ssl-tunnel-connect.sslvpn</ssl-tunnel-url>'
                                '<ip-address>10.77.0.2</ip-address><netmask>255.255.255.0</netmask>'
                                '<access-routes><member>10.77.0.0/24</member></access-routes></response>')
                    elif path == "hipreportcheck.esp":
                        assert fields["client-ip"] == ["10.77.0.2"]
                        body = "<response><hip-report-needed>no</hip-report-needed></response>"
                    elif path == "ssl-tunnel-connect.sslvpn":
                        assert query == {"user": ["alice"], "authcookie": [COOKIE]}
                        frames(stream, reader, calls)
                        continue
                    elif path == "logout.esp":
                        assert (query or fields)["authcookie"] == [COOKIE]
                        body = '<response status="success"/>'
                    else:
                        raise AssertionError(path)
                    response = (f"HTTP/1.1 200 OK\r\nContent-Length: {len(body)}\r\n"
                                f"Connection: close\r\n\r\n{body}")
                    stream.sendall(response.encode())
                    if path == "logout.esp":
                        return
    except Exception as error:
        errors.append(error)


def openconnect_probe():
    """OpenConnect's script-tun socket carries one raw IP packet per datagram."""
    sock = socket.socket(fileno=int(os.environ["VPNFD"]))
    sock.settimeout(10)
    icmp = bytearray(bytes.fromhex("0800000012340001") + b"openconnect")
    icmp[2:4] = struct.pack("!H", checksum(icmp))
    ip = bytearray(struct.pack("!BBHHHBBH4s4s", 0x45, 0, 20 + len(icmp), 1, 0, 64, 1, 0, LOCAL, REMOTE))
    ip[10:12] = struct.pack("!H", checksum(ip))
    sock.sendall(ip + icmp)
    reply = sock.recv(1500)
    assert reply[12:20] == REMOTE + LOCAL and reply[20] == 0
    assert checksum(reply[:20]) == checksum(reply[20:]) == 0
    assert reply[24:] == icmp[4:]
    Path(sys.argv[2]).touch()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--openconnect", metavar="PATH")
    group.add_argument("--vm", choices=["arm64", "amd64"])
    args = parser.parse_args()
    with tempfile.TemporaryDirectory() as directory:
        tmp = Path(directory)
        conf = tmp / "openssl.cnf"
        cert, key = tmp / "cert.pem", tmp / "key.pem"
        conf.write_text("[req]\ndistinguished_name=dn\nx509_extensions=v3\n"
                        "[dn]\n[v3]\nsubjectAltName=DNS:localhost,IP:10.0.2.2\n")
        subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                        "-config", conf, "-subj", "/CN=localhost", "-keyout", key, "-out", cert],
                       check=True, capture_output=True)
        fingerprint = subprocess.check_output(
            ["openssl", "x509", "-in", cert, "-noout", "-fingerprint", "-sha256"], text=True)
        pin = fingerprint.strip().split("=", 1)[1].replace(":", "")
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        ctx.load_cert_chain(cert, key)
        with socket.create_server(("127.0.0.1", 0)) as listener:
            listener.settimeout(110 if args.vm else 25)
            port = listener.getsockname()[1]
            calls, errors = [], []
            thread = threading.Thread(target=serve, args=(ctx, listener, calls, errors), daemon=True)
            thread.start()
            if args.openconnect:
                marker = tmp / "reply"
                script = shlex.join([sys.executable, __file__, "--probe", str(marker)])
                with subprocess.Popen(
                    [args.openconnect, "--protocol=gp", "--non-inter", "--passwd-on-stdin",
                     "--user=alice", "--os=linux", "--no-dtls", "--cafile", cert,
                     "--script-tun", "--script", script, f"https://localhost:{port}/gateway"],
                    stdin=subprocess.PIPE, text=True,
                ) as client:
                    client.stdin.write(PASSWORD + "\n")
                    client.stdin.close()
                    deadline = time.monotonic() + 10
                    while not marker.exists() and client.poll() is None and time.monotonic() < deadline:
                        time.sleep(0.05)
                    if client.poll() is None:
                        client.terminate()
                    try:
                        client.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        client.kill()
                        client.wait()
                assert not errors, errors
                assert marker.exists(), "OpenConnect did not carry an echo reply"
                print("OpenConnect received an ICMP echo reply with valid checksums and matching payload")
            elif args.vm:
                subprocess.run(["sh", ROOT / "scripts/crypta-gp-vm-gate.sh", args.vm, str(port), pin],
                               check=True, timeout=110)
            else:
                target = ROOT / "crypta/target/debug"
                result = subprocess.run(
                    [target / "examples/ac_harness", target / "atrium-crypta-ac", "localhost",
                     f"127.0.0.1:{port}", cert, "--gp", "--pin", pin, "--ping", "10.77.0.1",
                     "username=alice", f"password=@held:{PASSWORD}"],
                    env={**os.environ, "HARNESS_REBIND": "1"}, check=True, timeout=25,
                    capture_output=True, text=True,
                )
                print(result.stdout, result.stderr, sep="")
                assert "harness: ok" in result.stdout
                assert result.stdout.count("echo reply from 10.77.0.1") == 2
            thread.join(2)
            assert not errors, errors
            exchange = ["getconfig.esp", "hipreportcheck.esp", "ssl-tunnel-connect.sslvpn", "echo"]
            expected = ["prelogin.esp", "login.esp"] + exchange * (1 if args.openconnect else 2)
            assert calls == expected + ["logout.esp"], calls
    print("GlobalProtect TLS packet check passed")


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--probe":
        openconnect_probe()
    else:
        main()

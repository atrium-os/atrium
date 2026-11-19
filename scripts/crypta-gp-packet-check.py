#!/usr/bin/env python3
"""GlobalProtect TLS packet peer for the Crypta host and FreeBSD checks."""

import argparse
import base64
from contextlib import ExitStack
import hashlib
import hmac
import os
from pathlib import Path
import select
import shlex
import socket
import ssl
import struct
import subprocess
import sys
import tempfile
import threading
import time
import xml.etree.ElementTree as ET
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


class EspPeer:
    def __init__(self, socket, drop):
        self.socket = socket
        self.drop = drop
        self.configs = []
        self.errors = []
        self.echo = []
        self.probes = 0

    def offer(self):
        spi = 0x11223300 + len(self.configs) * 2
        incoming, outgoing = self.association(spi), self.association(spi + 1)
        self.configs.append((incoming, outgoing))
        fields = ("<gw-address>10.77.0.1</gw-address><ipsec><udp-port>4501</udp-port>"
                  "<ipsec-mode>esp-tunnel</ipsec-mode><enc-algo>aes-128-cbc</enc-algo><hmac-algo>sha1</hmac-algo>")
        for direction, assoc in [("c2s", incoming), ("s2c", outgoing)]:
            spi, ekey, akey = assoc
            fields += (f"<{direction}-spi>0x{spi:08x}</{direction}-spi>"
                       f"<ekey-{direction}><bits>128</bits><val>{ekey.hex()}</val></ekey-{direction}>"
                       f"<akey-{direction}><bits>160</bits><val>{akey.hex()}</val></akey-{direction}>")
        return fields + "</ipsec>"

    @staticmethod
    def association(spi):
        return spi, os.urandom(16), os.urandom(20)

    @staticmethod
    def cipher(key, iv, packet, decrypt=False):
        args = ["openssl", "enc", "-aes-128-cbc", "-nopad", "-nosalt",
                "-K", key.hex(), "-iv", iv.hex()]
        if decrypt:
            args.append("-d")
        return subprocess.run(args, input=packet, capture_output=True, check=True, timeout=3).stdout

    def serve(self):
        replay = {}
        sequence = {}
        try:
            while True:
                wire, address = self.socket.recvfrom(2048)
                spi = int.from_bytes(wire[:4], "big")
                seq = int.from_bytes(wire[4:8], "big")
                pair = next((entry for entry in self.configs if entry[0][0] == spi), None)
                assert pair is not None, f"unoffered ESP SPI {spi:08x}"
                (spi_in, ekey, akey), (spi_out, reply_ekey, reply_akey) = pair
                # OpenConnect sends its bootstrap probe with sequence zero.
                assert (spi_in, seq) not in replay, "client replayed an ESP sequence"
                assert hmac.compare_digest(wire[-12:], hmac.digest(akey, wire[:-12], hashlib.sha1)[:12])
                replay[spi_in, seq] = True
                plain = self.cipher(ekey, wire[8:24], wire[24:-12], decrypt=True)
                assert plain[-1] == 4
                padding = plain[-2]
                assert plain[len(plain) - padding - 2:-2] == bytes(range(1, padding + 1))
                packet = plain[:len(plain) - padding - 2]
                offset = (packet[0] & 15) * 4
                probe = packet[offset + 8:].startswith(b"monitor\0\0pan ha ")
                if probe:
                    self.probes += 1
                if self.drop:
                    continue
                reply = echo_reply(packet)
                padding = (-len(reply) - 2) % 16
                reply += bytes(range(1, padding + 1)) + bytes([padding, 4])
                sequence[spi_out] = sequence.get(spi_out, 0) + 1
                iv = os.urandom(16)
                frame = struct.pack("!II", spi_out, sequence[spi_out]) + iv + self.cipher(reply_ekey, iv, reply)
                self.socket.sendto(frame + hmac.digest(reply_akey, frame, hashlib.sha1)[:12], address)
                if not probe:
                    self.echo.append(spi_in)
        except (TimeoutError, OSError):
            return
        except Exception as error:
            self.errors.append(error)


def serve(ctx, listener, calls, errors, *, isolated=False, portal=False, esp=None, hip=None, receipts=None):
    done = threading.Event()
    workers = []

    def handle(raw):
        try:
            with ctx.wrap_socket(raw, server_side=True) as stream:
                stream.settimeout(15)
                with stream.makefile("rb") as reader:
                    first = reader.readline().decode().split()
                    if not first:
                        return
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
                    extra = ""
                    if isolated and not portal:
                        assert "cookie" not in headers, "portal cookie reached the gateway"
                        for values in (*fields.values(), *query.values()):
                            assert not {"portal-alice", "portal-secret", "portal-token"} & set(values), (
                                "portal credentials reached the gateway"
                            )
                    if path == "prelogin.esp":
                        if portal:
                            assert parsed.path == "/global-protect/prelogin.esp"
                            extra = "Set-Cookie: portal_only=portal-marker; Path=/; Secure; HttpOnly\r\n"
                        assert (query or fields)["clientos"] == ["Linux"]
                        body = ("<prelogin-response><status>Success</status>"
                                "<username-label>Username</username-label>"
                                "<password-label>Password</password-label></prelogin-response>")
                    elif portal:
                        assert parsed.path == "/global-protect/getconfig.esp"
                        assert fields.get("user") == ["portal-alice"] and fields.get("passwd") == ["portal-secret"]
                        assert headers.get("cookie") == "portal_only=portal-marker"
                        body = ('<policy><gateways><external><list>'
                                '<entry name="198.51.100.52:449"><description>Fixture gateway</description></entry>'
                                '</list></external></gateways><portal-userauthcookie>portal-token</portal-userauthcookie></policy>')
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
                                '<access-routes><member>10.77.0.0/24</member></access-routes>'
                                + (esp.offer() if esp else '') + '</response>')
                    elif path == "hipreportcheck.esp":
                        assert fields["client-ip"] == ["10.77.0.2"]
                        needed = "yes" if hip else "no"
                        interval = "<hip-report-interval>5</hip-report-interval>" if hip in ("refresh", "posture", "tessera", "tessera-domain") else ""
                        body = f"<response><hip-report-needed>{needed}</hip-report-needed>{interval}</response>"
                    elif path == "hipreport.esp":
                        assert hip in ("approve", "refresh", "posture", "tessera", "tessera-domain"), "HIP submitted without approval"
                        assert fields["authcookie"] == [COOKIE]
                        report, = fields["report"]
                        doc = ET.fromstring(report)
                        assert doc.tag == "hip-report" and doc.attrib == {"name": "hip-report"}
                        assert doc.findtext("user-name") == "alice"
                        assert doc.findtext("host-name") == "atrium"
                        assert doc.findtext("ip-address") == "10.77.0.2"
                        assert doc.findtext("hip-report-version") == "4"
                        categories = doc.findall("./categories/entry")
                        assert [node.attrib for node in categories[:2]] == [{"name": "host-info"}, {"name": "firewall"}]
                        if hip in ("tessera", "tessera-domain"):
                            disk = doc.find("./categories/entry[@name='disk-encryption']")
                            tessera = [entry for entry in disk.findall("entry")
                                       if entry.findtext("product") == "Atrium Tessera"]
                            assert len(tessera) == 1
                            sealed = tessera[0].findtext("mounted-sealed-volumes")
                            domains = tessera[0].findtext("verified-encrypted-domains")
                            if hip == "tessera-domain":
                                assert sealed == "0"
                                tessera_count = domains
                            else:
                                assert domains in (None, "0")
                                tessera_count = sealed
                            assert tessera_count in ("0", "1")
                            tags = {"product", "mounted-sealed-volumes"}
                            if domains is not None:
                                tags.add("verified-encrypted-domains")
                            assert {node.tag for node in tessera[0]} == tags
                        if hip == "posture":
                            assert [node.attrib for node in categories[2:]] == [
                                {"name": "disk-encryption"}, {"name": "patch-management"}, {"name": "screen-lock"},
                            ]
                            assert categories[2].findtext("entry/product") == "FreeBSD GELI"
                            assert categories[2].findtext("entry/mounted-encrypted-providers") == "1"
                            assert categories[3].findtext("last-check-unix") == "1760000000"
                            assert categories[3].findtext("catalog-updates-listed") == "0"
                            locked = categories[4].findtext("active-session-locked")
                            assert locked in ("yes", "no")
                        assert {node.tag for node in categories[0]} == {"os", "client-version", "ipv6-forwarding"}
                        forwarding = categories[0].findtext("ipv6-forwarding")
                        assert forwarding in ("yes", "no")
                        assert categories[1].findtext("entry/product") == "Atrium pf"
                        assert categories[1].findtext("entry/is-enabled") == "yes"
                        assert categories[0].findtext("os").startswith("FreeBSD ")
                        assert categories[0].findtext("client-version") == "Atrium"
                        digest, = fields["md5"]
                        assert doc.findtext("md5-sum") == digest
                        original = report.replace(f"<md5-sum>{digest}</md5-sum>", "", 1)
                        assert hashlib.md5(original.encode()).hexdigest() == digest
                        if receipts:
                            with receipts.open("a") as receipt:
                                fact = forwarding
                                if hip == "posture":
                                    fact = locked
                                elif hip in ("tessera", "tessera-domain"):
                                    fact = tessera_count
                                print(f"{fact} {digest}", file=receipt)
                        body = '<response status="success"/>'
                    elif path == "ssl-tunnel-connect.sslvpn":
                        assert query == {"user": ["alice"], "authcookie": [COOKIE]}
                        frames(stream, reader, calls)
                        return
                    elif path == "logout.esp":
                        assert (query or fields)["authcookie"] == [COOKIE]
                        body = '<response status="success"/>'
                    else:
                        raise AssertionError(path)
                    response = (f"HTTP/1.1 200 OK\r\nContent-Length: {len(body)}\r\n"
                                f"Connection: close\r\n{extra}\r\n{body}")
                    stream.sendall(response.encode())
                    if path == "logout.esp" or (portal and path == "getconfig.esp"):
                        done.set()
        except Exception as error:
            errors.append(error)
            done.set()

    deadline = time.monotonic() + listener.gettimeout()
    try:
        while not done.is_set() and time.monotonic() < deadline:
            if not select.select([listener], [], [], 0.2)[0]:
                continue
            raw, _ = listener.accept()
            worker = threading.Thread(target=handle, args=(raw,), daemon=True)
            worker.start()
            workers.append(worker)
        for worker in workers:
            worker.join(1)
    except OSError as error:
        if not done.is_set():
            errors.append(error)


def openconnect_probe():
    """OpenConnect's script-tun socket carries one raw IP packet per datagram."""
    sock = socket.socket(fileno=int(os.environ["VPNFD"]))
    sock.settimeout(0.3)
    icmp = bytearray(bytes.fromhex("0800000012340001") + b"openconnect")
    icmp[2:4] = struct.pack("!H", checksum(icmp))
    ip = bytearray(struct.pack("!BBHHHBBH4s4s", 0x45, 0, 20 + len(icmp), 1, 0, 64, 1, 0, LOCAL, REMOTE))
    ip[10:12] = struct.pack("!H", checksum(ip))
    for _ in range(16):
        sock.sendall(ip + icmp)
        try:
            reply = sock.recv(1500)
            break
        except TimeoutError:
            continue
    else:
        raise TimeoutError("OpenConnect received no echo reply")
    assert reply[12:20] == REMOTE + LOCAL and reply[20] == 0
    assert checksum(reply[:20]) == checksum(reply[20:]) == 0
    assert reply[24:] == icmp[4:]
    Path(sys.argv[2]).touch()



def two_origin_gate(tmp, arch):
    # One trust root and distinct TLS identities for the portal and gateway.
    ca = tmp / "ca.pem"
    ca_key = tmp / "ca.key"
    subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                    "-subj", "/CN=Crypta integration test CA", "-addext", "basicConstraints=critical,CA:TRUE",
                    "-addext", "keyUsage=critical,keyCertSign", "-keyout", ca_key, "-out", ca],
                   check=True, capture_output=True)
    trust = base64.b64encode(ssl.PEM_cert_to_DER_cert(ca.read_text())).decode()
    errors, portal_calls, gateway_calls = [], [], []
    with socket.create_server(("127.0.0.1", 0)) as portal, socket.create_server(("127.0.0.1", 0)) as gateway:
        threads = []
        for listener, ip, index, calls, is_portal in [
            (portal, "198.51.100.54", "1", portal_calls, True),
            (gateway, "198.51.100.52", "2", gateway_calls, False),
        ]:
            key, csr, leaf, config = [tmp / f"{index}.{ext}" for ext in ("key", "csr", "pem", "cnf")]
            config.write_text(f"subjectAltName=IP:{ip}\nbasicConstraints=critical,CA:FALSE\nextendedKeyUsage=serverAuth\n")
            subprocess.run(["openssl", "req", "-new", "-newkey", "rsa:2048", "-nodes", "-subj", f"/CN={ip}",
                            "-keyout", key, "-out", csr], check=True, capture_output=True)
            subprocess.run(["openssl", "x509", "-req", "-in", csr, "-CA", ca, "-CAkey", ca_key,
                            "-set_serial", index, "-days", "1", "-extfile", config, "-out", leaf],
                           check=True, capture_output=True)
            ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            ctx.load_cert_chain(leaf, key)
            listener.settimeout(300)
            thread = threading.Thread(target=serve, args=(ctx, listener, calls, errors),
                                      kwargs={"isolated": True, "portal": is_portal}, daemon=True)
            thread.start()
            threads.append(thread)
        subprocess.run(["sh", ROOT / "scripts/crypta-gp-full-gate.sh", arch,
                        str(gateway.getsockname()[1]), trust, str(portal.getsockname()[1])],
                       check=True, timeout=300)
        for thread in threads:
            thread.join(2)
        assert not errors, errors
        assert portal_calls == ["prelogin.esp", "getconfig.esp"], portal_calls
        exchange = ["getconfig.esp", "hipreportcheck.esp", "ssl-tunnel-connect.sslvpn", "echo"]
        assert gateway_calls == ["prelogin.esp", "login.esp"] + exchange * 2 + ["logout.esp"], gateway_calls
        print("Portal and gateway saw separate credentials. One gateway login carried traffic before and after rebind.")

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    group = parser.add_mutually_exclusive_group()
    group.add_argument("--openconnect", metavar="PATH")
    group.add_argument("--vm", choices=["arm64", "amd64"])
    group.add_argument("--full-vm", choices=["arm64", "amd64"])
    group.add_argument("--portal-vm", choices=["arm64", "amd64"])
    parser.add_argument("--esp", action="store_true", help="advertise ESP and carry packets over UDP")
    parser.add_argument("--drop-esp", action="store_true", help="drop ESP probes to test TLS fallback")
    parser.add_argument("--hip", choices=["approve", "decline", "refresh", "posture", "tessera", "tessera-domain"], help="require a reviewed HIP report in the full gate")
    args = parser.parse_args()
    if args.hip and (not args.full_vm or args.drop_esp):
        parser.error("HIP cases require the full VM gate")
    if (args.esp or args.drop_esp) and (args.portal_vm or args.vm):
        parser.error("use --full-vm or the host client for ESP packet checks")
    with tempfile.TemporaryDirectory() as directory:
        tmp = Path(directory)
        if args.portal_vm:
            two_origin_gate(tmp, args.portal_vm)
            return
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
        with ExitStack() as stack:
            esp = None
            if args.esp or args.drop_esp:
                udp = stack.enter_context(socket.socket(socket.AF_INET, socket.SOCK_DGRAM))
                udp.bind(("127.0.0.1", 4501))
                udp.settimeout(300)
                esp = EspPeer(udp, args.drop_esp)
                threading.Thread(target=esp.serve, daemon=True).start()
            listener = stack.enter_context(socket.create_server(("127.0.0.1", 0)))
            listener.settimeout(300 if args.full_vm else 110 if args.vm else 25)
            port = listener.getsockname()[1]
            calls, errors = [], []
            receipts = tmp / "hip-receipts" if args.hip in ("refresh", "posture", "tessera", "tessera-domain") else None
            if receipts:
                receipts.touch()
            thread = threading.Thread(target=serve, args=(ctx, listener, calls, errors), kwargs={"esp": esp, "hip": args.hip, "receipts": receipts}, daemon=True)
            thread.start()
            if args.openconnect:
                marker = tmp / "reply"
                script = shlex.join([sys.executable, __file__, "--probe", str(marker)])
                with subprocess.Popen(
                    [args.openconnect, "--protocol=gp", "--non-inter", "--passwd-on-stdin",
                     "--user=alice", "--os=linux", *([] if esp else ["--no-dtls"]), "--cafile", cert,
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
            elif args.full_vm:
                subprocess.run(["sh", ROOT / "scripts/crypta-gp-full-gate.sh", args.full_vm, str(port), pin, "",
                                ("TLS" if args.drop_esp else "ESP") if esp else "", args.hip or "", str(receipts or "")],
                               check=True, timeout=300)
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
            exchange = ["getconfig.esp", "hipreportcheck.esp"]
            if args.hip == "decline":
                assert calls == ["prelogin.esp", "login.esp", *exchange], calls
                print("Declining HIP sent no report and opened no data tunnel")
                return
            if args.hip in ("refresh", "posture", "tessera", "tessera-domain"):
                assert calls.count("getconfig.esp") == 1, calls
                if esp:
                    assert not esp.errors, esp.errors
                    assert len(esp.echo) == 2 and len(set(esp.echo)) == 1, esp.echo
                    assert "ssl-tunnel-connect.sslvpn" not in calls, calls
                else:
                    assert calls.count("ssl-tunnel-connect.sslvpn") == 1 and calls.count("echo") == 2, calls
                assert calls[-1] == "logout.esp", calls
                print("HIP refresh preserved the original data connection")
                return
            if args.hip == "approve":
                exchange.append("hipreport.esp")
            if esp:
                assert not esp.errors, esp.errors
                assert esp.probes > 0, "client sent no authenticated ESP probes"
                if not args.drop_esp:
                    assert len(set(esp.echo)) == (1 if args.openconnect else 2), esp.echo
                    print("ESP carried packet echo on each negotiated association")
            if not esp or args.drop_esp:
                exchange += ["ssl-tunnel-connect.sslvpn", "echo"]
            expected = ["prelogin.esp", "login.esp"] + exchange * (1 if args.openconnect else 2)
            assert calls == expected + ["logout.esp"], calls
    print("GlobalProtect TLS packet check passed")


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--probe":
        openconnect_probe()
    else:
        main()

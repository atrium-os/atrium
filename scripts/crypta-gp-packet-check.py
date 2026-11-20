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


RECONNECT_HIP = ("tessera-reconnect", "tessera-relock-decline", "tessera-relock-reconnect", "tessera-final-lock-decline", "tessera-cycle")
TESSERA_HIP = ("tessera", "tessera-domain", "tessera-rebind", "tessera-rebind-decline", *RECONNECT_HIP)
REBIND_HIP_RECHECK = "esp-loss-hip-submit-rebind-recheck"
REBIND_HIP_DECLINE = "esp-loss-hip-submit-rebind-decline"
FINAL_HIP_RECOVER = "esp-loss-hip-submit-final-recover"
FINAL_HIP_FALLBACK = ("esp-loss-hip-submit-final-fallback", FINAL_HIP_RECOVER)
REBIND_HIP_RECONNECT = ("esp-loss-hip-submit-rebind-reconnect", "esp-loss-hip-submit-final-rebind", *FINAL_HIP_FALLBACK)
REBIND_HIP_RETRY = ("esp-loss-hip-submit-rebind-retry", REBIND_HIP_RECHECK, REBIND_HIP_DECLINE, *REBIND_HIP_RECONNECT)
ESP_LOSS_RECONNECT_HIP = (
    *REBIND_HIP_RETRY, "esp-loss-hip-submit-retry-reconnect", "esp-loss-hip-submit-retry-decline", "esp-loss-hip-submit-retry-recheck", "esp-loss-reconnect", "esp-loss-rebind-decline", "esp-loss-rebind-reconnect", "esp-loss-hip-submit-retry",
)
ESP_LOSS_HIP = ("esp-loss", "esp-loss-decline", *ESP_LOSS_RECONNECT_HIP)
REFRESH_HIP = ("refresh", "posture", *ESP_LOSS_HIP, *TESSERA_HIP)


def require_reconnect_approval(receipts, config_index):
    if config_index == 0:
        return
    assert config_index >= 2, "data or HIP report used the declined configuration"
    suffix = ".approved" if config_index < 4 else ".relock-approved" if config_index < 6 else ".final-approved"
    assert receipts.with_suffix(suffix).exists(), f"configuration {config_index + 1} used before HIP approval"


# The cycle declines offer 1. The later reconnects use offers 2, 4, and 6.
CYCLE_RECONNECTS = frozenset((2, 4, 6))
CYCLE_REBINDS = frozenset((3, 5, 7))


class EspPeer:
    def __init__(self, socket, drop, reconnect_approval=None, drop_config_indices=(), lose_active=False, loss_reconnect_approval=None):
        self.socket = socket
        self.drop = drop
        self.drop_config_indices = drop_config_indices
        self.lose_active = lose_active
        self.loss_reconnect_approval = loss_reconnect_approval
        self.dropped_probe_spis = set()
        self.reconnect_approval = reconnect_approval
        self.configs = []
        self.errors = []
        self.echo = []
        self.probes = 0
        self.received_spis = set()
        self.hip_reported_spis = set()

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
                self.received_spis.add(spi_in)
                if self.reconnect_approval:
                    config_index = self.configs.index(pair)
                    require_reconnect_approval(self.reconnect_approval, config_index)
                    if config_index >= 2:
                        assert spi_in in self.hip_reported_spis, "ESP opened before HIP submission"
                if self.loss_reconnect_approval and pair in self.configs[2:]:
                    assert self.loss_reconnect_approval.exists(), "ESP resumed before fresh approval"
                    if pair in self.configs[4:] and not self.loss_reconnect_approval.with_suffix(".posture-restored").exists():
                        assert self.loss_reconnect_approval.with_suffix(".rebind-approved").exists(), "ESP resumed before rebound reconnect approval"
                    assert spi_in in self.hip_reported_spis, "ESP resumed before the approved HIP report"
                plain = self.cipher(ekey, wire[8:24], wire[24:-12], decrypt=True)
                assert plain[-1] == 4
                padding = plain[-2]
                assert plain[len(plain) - padding - 2:-2] == bytes(range(1, padding + 1))
                packet = plain[:len(plain) - padding - 2]
                offset = (packet[0] & 15) * 4
                probe = packet[offset + 8:].startswith(b"monitor\0\0pan ha ")
                if probe:
                    self.probes += 1
                if self.configs.index(pair) in self.drop_config_indices:
                    assert probe, "ESP carried data on an association that should fall back"
                    self.dropped_probe_spis.add(spi_in)
                    continue
                if self.lose_active and self.echo and pair == self.configs[0]:
                    assert probe, "ESP data arrived during the active outage"
                    self.dropped_probe_spis.add(spi_in)
                    continue
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
                        if hip in ("esp-loss-hip-submit-retry-recheck", "esp-loss-hip-submit-retry-decline", "esp-loss-hip-submit-retry-reconnect", *REBIND_HIP_RETRY) and calls.count("getconfig.esp") == 4:
                            deadline = time.monotonic() + 25
                            while not receipts.with_suffix(".posture-restored").exists() and time.monotonic() < deadline:
                                time.sleep(0.05)
                            assert receipts.with_suffix(".posture-restored").exists(), "retry posture did not change"
                        if hip in (REBIND_HIP_RECHECK, REBIND_HIP_DECLINE, *REBIND_HIP_RECONNECT) and calls.count("getconfig.esp") == 7:
                            deadline = time.monotonic() + 25
                            while not receipts.with_suffix(".rebind-posture").exists() and time.monotonic() < deadline:
                                time.sleep(0.05)
                            assert receipts.with_suffix(".rebind-posture").exists(), "rebind posture did not change"
                        needed = "yes" if hip else "no"
                        interval = "<hip-report-interval>5</hip-report-interval>" if hip in REFRESH_HIP else ""
                        body = f"<response><hip-report-needed>{needed}</hip-report-needed>{interval}</response>"
                    elif path == "hipreport.esp":
                        assert hip in ("approve", *REFRESH_HIP), "HIP submitted without approval"
                        assert fields["authcookie"] == [COOKIE]
                        if hip in RECONNECT_HIP:
                            require_reconnect_approval(receipts, calls.count("getconfig.esp") - 1)
                        if hip in ESP_LOSS_RECONNECT_HIP and calls.count("getconfig.esp") >= 3:
                            assert receipts.with_suffix(".approved").exists(), calls
                            if calls.count("getconfig.esp") >= 5 and hip != "esp-loss-hip-submit-retry-recheck":
                                assert receipts.with_suffix(".rebind-approved").exists(), calls
                        report, = fields["report"]
                        doc = ET.fromstring(report)
                        assert doc.tag == "hip-report" and doc.attrib == {"name": "hip-report"}
                        assert doc.findtext("user-name") == "alice"
                        assert doc.findtext("host-name") == "atrium"
                        assert doc.findtext("ip-address") == "10.77.0.2"
                        assert doc.findtext("hip-report-version") == "4"
                        categories = doc.findall("./categories/entry")
                        assert [node.attrib for node in categories[:2]] == [{"name": "host-info"}, {"name": "firewall"}]
                        if hip in TESSERA_HIP:
                            disk = doc.find("./categories/entry[@name='disk-encryption']")
                            tessera = [entry for entry in disk.findall("entry")
                                       if entry.findtext("product") == "Atrium Tessera"]
                            assert len(tessera) == 1
                            sealed = tessera[0].findtext("mounted-sealed-volumes")
                            domains = tessera[0].findtext("verified-encrypted-domains")
                            if hip != "tessera":
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
                        if hip == FINAL_HIP_RECOVER and calls.count("getconfig.esp") == 10:
                            initial_forwarding = receipts.read_text().splitlines()[0].split()[0]
                            if forwarding == initial_forwarding:
                                assert receipts.with_suffix(".recovery-approved").exists(), "recovered ESP uploaded restored forwarding before approval"
                        if receipts:
                            with receipts.open("a") as receipt:
                                fact = forwarding
                                if hip == "posture":
                                    fact = locked
                                elif hip in TESSERA_HIP:
                                    fact = tessera_count
                                print(f"{fact} {digest}", file=receipt)
                        if hip in ("esp-loss-hip-submit-retry-decline", "esp-loss-hip-submit-retry-reconnect", *REBIND_HIP_RETRY):
                            assert calls.count("getconfig.esp") != 4, "declined retry submitted HIP"
                        if hip in (*REBIND_HIP_RETRY, "esp-loss-hip-submit-retry-reconnect", "esp-loss-hip-submit-retry-decline", "esp-loss-hip-submit-retry", "esp-loss-hip-submit-retry-recheck"):
                            config_count = calls.count("getconfig.esp")
                            if config_count == 3 or (config_count == 6 and hip in REBIND_HIP_RETRY):
                                calls.append("hip-submit-response-dropped")
                                return
                            if config_count == 4 and hip == "esp-loss-hip-submit-retry-recheck":
                                assert receipts.with_suffix(".retry").exists(), "retry submitted HIP before fresh approval"
                            if config_count == 4:
                                deadline = time.monotonic() + 25
                                while not receipts.with_suffix(".retry").exists() and time.monotonic() < deadline:
                                    time.sleep(0.05)
                                assert receipts.with_suffix(".retry").exists(), "HIP retry was never released"
                        if hip in REBIND_HIP_RETRY and config_count == 7:
                            assert hip not in (REBIND_HIP_DECLINE, *REBIND_HIP_RECONNECT), "declined rebind retry submitted HIP"
                            if hip == REBIND_HIP_RECHECK:
                                assert receipts.with_suffix(".rebind-retry").exists(), "rebind uploaded before fresh approval"
                            deadline = time.monotonic() + 25
                            while not receipts.with_suffix(".rebind-retry").exists() and time.monotonic() < deadline:
                                time.sleep(0.05)
                            assert receipts.with_suffix(".rebind-retry").exists(), "rebind retry was never released"
                        if hip in REBIND_HIP_RECONNECT and config_count >= 8:
                            assert receipts.with_suffix(".rebind-retry").exists(), "reconnect uploaded before fresh approval"
                        if esp and (hip in RECONNECT_HIP or hip in ESP_LOSS_RECONNECT_HIP):
                            esp.hip_reported_spis.add(esp.configs[-1][0][0])
                        body = '<response status="success"/>'
                    elif path == "ssl-tunnel-connect.sslvpn":
                        if hip in RECONNECT_HIP:
                            require_reconnect_approval(receipts, calls.count("getconfig.esp") - 1)
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
    parser.add_argument("--drop-esp-after-initial", action="store_true",
                        help="use ESP initially and force TLS fallback on later Tessera connections")
    parser.add_argument("--recover-esp-on-rebind", action="store_true",
                        help="force TLS on Tessera reconnects and restore ESP on rebinds")
    parser.add_argument("--drop-esp-on-rebind", action="store_true",
                        help="keep ESP on reconnect and force TLS fallback on Tessera rebinds")
    parser.add_argument("--hip", choices=("approve", "decline", *REFRESH_HIP), help="require a reviewed HIP report in the full gate")
    args = parser.parse_args()
    outages = [indices for enabled, indices in (
        (args.drop_esp_after_initial, CYCLE_RECONNECTS | CYCLE_REBINDS),
        (args.recover_esp_on_rebind, CYCLE_RECONNECTS),
        (args.drop_esp_on_rebind, CYCLE_REBINDS),
    ) if enabled]
    if len(outages) > 1:
        parser.error("choose one staged ESP outage")
    if outages and (args.hip != "tessera-cycle" or args.drop_esp):
        parser.error("staged ESP outages require --hip tessera-cycle")
    drop_config_indices = outages[0] if outages else frozenset()
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
        receipts = tmp / "hip-receipts" if args.hip in REFRESH_HIP else None
        reconnect_approval = receipts.with_suffix(".approved") if args.hip in RECONNECT_HIP else None
        if args.hip in FINAL_HIP_FALLBACK:
            drop_config_indices = (8,)
        with ExitStack() as stack:
            esp = None
            if args.esp or args.drop_esp or drop_config_indices or args.hip in ESP_LOSS_HIP:
                udp = stack.enter_context(socket.socket(socket.AF_INET, socket.SOCK_DGRAM))
                udp.bind(("127.0.0.1", 4501))
                udp.settimeout(300)
                esp = EspPeer(udp, args.drop_esp, reconnect_approval, drop_config_indices,
                              lose_active=args.hip in ESP_LOSS_HIP,
                              loss_reconnect_approval=receipts.with_suffix(".approved") if args.hip in ESP_LOSS_RECONNECT_HIP else None)
                threading.Thread(target=esp.serve, daemon=True).start()
            listener = stack.enter_context(socket.create_server(("127.0.0.1", 0)))
            listener.settimeout(300 if args.full_vm else 110 if args.vm else 25)
            port = listener.getsockname()[1]
            calls, errors = [], []
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
            if args.hip in RECONNECT_HIP:
                declined_relock = args.hip == "tessera-relock-decline"
                final_reconnect = args.hip == "tessera-cycle"
                declined_final_lock = args.hip == "tessera-final-lock-decline" or final_reconnect
                second_reconnect = args.hip in ("tessera-relock-reconnect", "tessera-final-lock-decline") or final_reconnect
                config_count = 8 if final_reconnect else 6 if second_reconnect else 4
                configs = [i for i, call in enumerate(calls) if call == "getconfig.esp"]
                assert len(configs) == config_count and calls.count("login.esp") == 1, calls
                assert calls[configs[1]:configs[2]] == exchange, calls
                reconnect_calls = calls[configs[2]:configs[3]]
                rebound_end = configs[4] if second_reconnect else len(calls)
                final_calls = calls[configs[3]:rebound_end]
                if second_reconnect:
                    second_calls = calls[rebound_end:configs[5]]
                    second_rebind_calls = calls[configs[5]:configs[6] if final_reconnect else len(calls)]
                    assert second_calls[:3] == [*exchange, "hipreport.esp"], calls
                    assert second_rebind_calls[:3] == [*exchange, "hipreport.esp"], calls
                    expected_end = "hipreportcheck.esp" if declined_final_lock else "logout.esp"
                    assert second_rebind_calls[-1] == expected_end, calls
                    assert final_calls[-1] == "hipreportcheck.esp", calls
                if final_reconnect:
                    third_calls = calls[configs[6]:configs[7]]
                    last_rebind_calls = calls[configs[7]:]
                    assert all(phase[:3] == [*exchange, "hipreport.esp"]
                               for phase in (third_calls, last_rebind_calls)), calls
                    assert last_rebind_calls[-1] == "logout.esp", calls
                assert reconnect_calls[:3] == [*exchange, "hipreport.esp"], calls
                assert final_calls[:3] == [*exchange, "hipreport.esp"], calls
                if declined_relock:
                    assert final_calls[-1] == "hipreportcheck.esp", calls
                elif not second_reconnect:
                    assert final_calls[-1] == "logout.esp", calls
                reports = [line.split() for line in receipts.read_text().splitlines()]
                runs = []
                for report in reports:
                    if not runs or runs[-1][0] != report:
                        runs.append([])
                    runs[-1].append(report)
                expected_counts = ["0", "1", "0", "1"]
                if not declined_relock:
                    expected_counts.append("0")
                if second_reconnect:
                    expected_counts.append("1")
                    if not declined_final_lock:
                        expected_counts.append("0")
                if final_reconnect:
                    expected_counts.extend(("0", "1"))
                assert [run[0][0] for run in runs] == expected_counts, reports
                if final_reconnect:
                    assert len(runs[6]) >= 2 and len(runs[7]) >= 2, reports
                    assert runs[4][0] == runs[6][0] and runs[5][0] == runs[7][0], reports
                if second_reconnect:
                    assert len(runs[4]) >= 2 and len(runs[5]) >= 2 and runs[3][0] == runs[5][0], reports
                    if not declined_final_lock:
                        assert runs[4][0] == runs[6][0], reports
                assert len(runs[2]) >= 2 and len(runs[3]) >= 2 and runs[1][0] == runs[3][0], reports
                if not declined_relock:
                    assert runs[2][0] == runs[4][0], reports
                rebound_echoes = 1 if declined_relock or second_reconnect else 2
                second_rebind_echoes = 1 if declined_final_lock else 2
                phase_echoes = [2, 0, 3, rebound_echoes]
                if second_reconnect:
                    phase_echoes.extend((3, second_rebind_echoes))
                if final_reconnect:
                    phase_echoes.extend((3, 1))
                active_indices = {i for i, echoes in enumerate(phase_echoes) if echoes}
                tls_indices = active_indices & drop_config_indices if esp else active_indices
                if esp:
                    assert not esp.errors, esp.errors
                    assert len(esp.configs) == config_count, calls
                    spis = [config[0][0] for config in esp.configs]
                    assert esp.received_spis == {spis[i] for i in active_indices}, esp.received_spis
                    assert esp.dropped_probe_spis == {spis[i] for i in tls_indices}, esp.dropped_probe_spis
                    assert esp.echo == [spis[i] for i, echoes in enumerate(phase_echoes)
                                        if i not in tls_indices for _ in range(echoes)], esp.echo
                for i, (start, end) in enumerate(zip(configs, [*configs[1:], len(calls)])):
                    data_calls = [call for call in calls[start:end]
                                  if call in ("ssl-tunnel-connect.sslvpn", "echo")]
                    expected_data = ["ssl-tunnel-connect.sslvpn"] + ["echo"] * phase_echoes[i] if i in tls_indices else []
                    assert data_calls == expected_data, (i, data_calls, expected_data)
                    if i in tls_indices:
                        phase = calls[start:end]
                        assert phase.index("hipreport.esp") < phase.index("ssl-tunnel-connect.sslvpn"), phase
                if final_reconnect:
                    if args.recover_esp_on_rebind:
                        print("Dropped probes forced TLS on reconnects and ESP carried echo again on rebinds")
                    elif args.drop_esp_on_rebind:
                        print("ESP carried echo on reconnects and dropped probes forced TLS on rebinds")
                    elif args.drop_esp_after_initial:
                        print("ESP carried the initial traffic, then dropped probes forced TLS on every reconnect and rebind")
                    print("Final reconnect verified decline, silent refresh, fresh unlock approval, and rebind")
                elif declined_final_lock:
                    print("Declining the final lock closed the second rebound tunnel without sending the changed count")
                elif second_reconnect:
                    print("The second rebound tunnel carried traffic after fresh approval of the locked report")
                elif declined_relock:
                    print("Declining the second key lock sent no changed count and closed the rebound tunnel")
                else:
                    print("The rebound tunnel carried traffic after the unchanged report and the approved key lock")
                return
            if args.hip == "tessera-rebind-decline":
                assert calls.count("login.esp") == 1 and calls.count("getconfig.esp") == 2, calls
                rebound = len(calls) - 1 - calls[::-1].index("getconfig.esp")
                after_rebind = calls[rebound:]
                assert "hipreportcheck.esp" in after_rebind, calls
                assert set(after_rebind) <= {"getconfig.esp", "hipreportcheck.esp", "logout.esp"}, calls
                counts = [line.split()[0] for line in receipts.read_text().splitlines()]
                first_change = counts.index("1")
                assert first_change and counts[:first_change] == ["0"] * first_change, counts
                assert all(count == "1" for count in counts[first_change:]), counts
                if esp:
                    assert not esp.errors, esp.errors
                    assert len(esp.configs) == 2 and esp.probes > 0, (len(esp.configs), esp.probes)
                    original_spi = esp.configs[0][0][0]
                    assert esp.received_spis == {original_spi} and esp.echo == [original_spi] * 2, esp.echo
                    assert "ssl-tunnel-connect.sslvpn" not in calls, calls
                else:
                    assert calls.count("ssl-tunnel-connect.sslvpn") == 1 and calls.count("echo") == 2, calls
                print("Declining the rebound review sent no changed HIP count and opened no rebound data tunnel")
                return
            if args.hip in ESP_LOSS_HIP:
                declined = args.hip == "esp-loss-decline"
                declined_rebind = args.hip == "esp-loss-rebind-decline"
                reconnected = args.hip in ESP_LOSS_RECONNECT_HIP
                reconnected_after_rebind = args.hip == "esp-loss-rebind-reconnect"
                retry_recheck = args.hip == "esp-loss-hip-submit-retry-recheck"
                retry_decline = args.hip == "esp-loss-hip-submit-retry-decline"
                rebind_retry = args.hip in REBIND_HIP_RETRY
                rebind_recheck = args.hip == REBIND_HIP_RECHECK
                rebind_decline = args.hip == REBIND_HIP_DECLINE
                rebind_reconnect = args.hip in REBIND_HIP_RECONNECT
                final_fallback = args.hip in FINAL_HIP_FALLBACK
                final_recover = args.hip == FINAL_HIP_RECOVER
                final_rebind_retry = final_fallback or args.hip == "esp-loss-hip-submit-final-rebind"
                retry_reconnect = rebind_retry or args.hip == "esp-loss-hip-submit-retry-reconnect"
                submit_retry = retry_reconnect or retry_decline or retry_recheck or args.hip == "esp-loss-hip-submit-retry"
                assert not esp.errors, esp.errors
                configs = [i for i, call in enumerate(calls) if call == "getconfig.esp"]
                assert len(esp.configs) == len(configs) == (11 if final_recover else 9 if final_rebind_retry else 8 if rebind_reconnect else 7 if rebind_retry else 6 if reconnected_after_rebind or retry_reconnect else 5 if retry_recheck else 4 if reconnected else 2), calls
                original_spi = esp.configs[0][0][0]
                expected_spis = [original_spi]
                if submit_retry:
                    if retry_reconnect:
                        expected_spis.extend([esp.configs[4][0][0]] * 2)
                        if not rebind_decline:
                            successful_spi = esp.configs[7 if rebind_reconnect else 6][0][0]
                            expected_spis.append(successful_spi)
                        if rebind_recheck or rebind_reconnect:
                            expected_spis.append(successful_spi)
                        if final_rebind_retry and not final_fallback:
                            expected_spis.append(esp.configs[8][0][0])
                        if final_recover:
                            expected_spis.extend([esp.configs[9][0][0]] * 3 + [esp.configs[10][0][0]])
                    elif not retry_decline:
                        expected_spis.extend([esp.configs[3][0][0]] * 2)
                    if retry_recheck:
                        expected_spis.append(esp.configs[4][0][0])
                elif reconnected:
                    expected_spis.extend([esp.configs[2][0][0]] * 2 + [esp.configs[3][0][0]])
                if reconnected_after_rebind:
                    expected_spis.extend([esp.configs[4][0][0]] * 3 + [esp.configs[5][0][0]])
                # The guest's failed ping can remain queued until the acknowledged
                # ESP retry starts. It must use the successful association.
                allowed_echoes = [expected_spis]
                if rebind_retry and not rebind_decline:
                    if final_recover:
                        allowed_echoes.append([*expected_spis[:-4], expected_spis[-5], *expected_spis[-4:]])
                    elif final_rebind_retry and not final_fallback:
                        allowed_echoes.append([*expected_spis[:-1], expected_spis[-2], expected_spis[-1]])
                    else:
                        allowed_echoes.append([*expected_spis, expected_spis[-1]])
                dropped_spis = {original_spi}
                received_spis = set(expected_spis)
                if final_fallback:
                    fallback_spi = esp.configs[8][0][0]
                    dropped_spis.add(fallback_spi)
                    received_spis.add(fallback_spi)
                assert esp.echo in allowed_echoes and esp.dropped_probe_spis == dropped_spis, esp.echo
                assert esp.received_spis == received_spis, esp.received_spis
                assert calls.count("login.esp") == 1 and calls[-1] == ("hipreportcheck.esp" if declined or declined_rebind or retry_decline or rebind_decline else "logout.esp"), calls
                recovery = configs[1]
                assert calls[recovery:recovery + 4] == [*exchange, "hipreport.esp", "ssl-tunnel-connect.sslvpn"], calls
                assert calls.count("ssl-tunnel-connect.sslvpn") == (2 if final_fallback else 1), calls
                assert calls.count("echo") == (3 if final_fallback else 1 if declined or reconnected else 2), calls
                reports = receipts.read_text().splitlines()
                report_index = calls[:recovery].count("hipreport.esp")
                if declined:
                    assert len(reports) >= report_index + 2 and len(set(reports)) == 1, reports
                    print("Declining changed HIP after ESP loss stopped the recovered TLS tunnel")
                    return
                original = reports[0]
                assert reports[report_index:report_index + 2] == [original, original], reports
                assert all(report == original for report in reports[:report_index]), reports
                if reconnected:
                    assert calls[configs[1]:configs[2]][-1] == "hipreportcheck.esp", calls
                    for config in configs[2:]:
                        if config == configs[3] and (retry_decline or retry_reconnect):
                            continue
                        if (rebind_decline or rebind_reconnect) and config == configs[6]:
                            continue
                        assert calls[config:config + 3] == [*exchange, "hipreport.esp"], calls
                    reconnect_report = calls[:configs[2]].count("hipreport.esp")
                    next_report = calls[:configs[3]].count("hipreport.esp")
                    assert all(report == original for report in reports[:reconnect_report]), reports
                    changed = reports[reconnect_report]
                    assert changed.split()[0] != original.split()[0], reports
                    if submit_retry:
                        assert calls[configs[2]:configs[3]] == [
                            *exchange, "hipreport.esp", "hip-submit-response-dropped",
                        ], calls
                        assert reports[reconnect_report:next_report] == [changed], reports
                        if retry_decline:
                            assert calls[configs[3]:] == exchange, calls
                            assert next_report == len(reports), reports
                            print("Declining the retry review withheld restored HIP and kept ESP down")
                            return
                        if retry_reconnect:
                            assert calls[configs[3]:configs[4]] == exchange, calls
                            final_rebind = calls[:configs[5]].count("hipreport.esp")
                            assert final_rebind >= next_report + 2, reports
                            if rebind_retry:
                                assert calls[configs[5]:configs[6]] == [
                                    *exchange, "hipreport.esp", "hip-submit-response-dropped",
                                ], calls
                                assert calls[:configs[6]].count("hipreport.esp") == final_rebind + 1, calls
                                if rebind_decline:
                                    assert calls[configs[6]:] == exchange, calls
                                    assert len(reports) == final_rebind + 1, reports
                                    assert set(reports[next_report:]) == {original}, reports
                                    print("Declining the rebind retry withheld changed HIP and kept ESP down")
                                    return
                                if rebind_reconnect:
                                    assert calls[configs[6]:configs[7]] == exchange, calls
                                if rebind_recheck or rebind_reconnect:
                                    retry_report = final_rebind + 1
                                    assert set(reports[next_report:retry_report]) == {original}, reports
                                    assert reports[retry_report:retry_report + 2] == [changed, changed], reports
                                    if not final_recover:
                                        assert set(reports[retry_report:]) == {changed}, reports
                                    if final_rebind_retry:
                                        final_phase_report = calls[:configs[8]].count("hipreport.esp")
                                        assert final_phase_report >= retry_report + 2, reports
                                        final_reports = reports[final_phase_report:]
                                        if not final_recover:
                                            assert set(final_reports) == {changed}, reports
                                        if final_fallback:
                                            assert calls[configs[8]:configs[8] + 4] == [
                                                *exchange, "hipreport.esp", "ssl-tunnel-connect.sslvpn",
                                            ], calls
                                            assert len(reports[final_phase_report:]) >= 2, reports
                                            if final_recover:
                                                recovery_report = calls[:configs[9]].count("hipreport.esp")
                                                assert recovery_report >= final_phase_report + 2, reports
                                                assert set(reports[final_phase_report:recovery_report]) == {changed}, reports
                                                recovered_reports = reports[recovery_report:]
                                                assert recovered_reports[:2] == [changed, changed], reports
                                                first_restored = recovered_reports.index(original)
                                                assert set(recovered_reports[:first_restored]) == {changed}, reports
                                                assert set(recovered_reports[first_restored:]) == {original}, reports
                                                restored_rebind_report = calls[:configs[10]].count("hipreport.esp")
                                                assert restored_rebind_report >= recovery_report + first_restored + 2, reports
                                                assert reports[restored_rebind_report:] and set(reports[restored_rebind_report:]) == {original}, reports
                                                print("Recovered ESP required fresh approval, refreshed silently, and rebound with the unchanged report")
                                            else:
                                                print("Final rebind fell back to TLS after unchanged HIP and refreshed without prompting")
                                        else:
                                            print("The final rebind submitted unchanged HIP before new ESP traffic")
                                    else:
                                        print("Rebind retry required fresh approval and refreshed the approved change over ESP")
                                else:
                                    assert set(reports[next_report:]) == {original}, reports
                                    print("Rebind retried unchanged HIP and opened ESP only after acknowledgement")
                            else:
                                assert set(reports[next_report:]) == {original}, reports
                                print("Reconnect after retry decline required fresh HIP approval, refreshed silently, and rebound on ESP")
                            return
                        if retry_recheck:
                            final_rebind = calls[:configs[4]].count("hipreport.esp")
                            assert final_rebind >= next_report + 2, reports
                            assert set(reports[next_report:]) == {original}, reports
                            print("Retry reviewed new posture, refreshed silently, and rebound on ESP")
                        else:
                            assert len(reports[next_report:]) >= 2 and set(reports[next_report:]) == {changed}, reports
                        print("HIP submit response dropped on ESP reconnect. Retry restored ESP and refreshed unchanged HIP.")
                        return
                    assert reports[reconnect_report:reconnect_report + 2] == [changed, changed], reports
                    assert reports[reconnect_report:next_report] == sorted(
                        reports[reconnect_report:next_report], key=lambda report: report == original), reports
                    assert set(reports[reconnect_report:next_report]) == {changed, original}, reports
                    if reconnected_after_rebind:
                        assert calls[configs[4] - 1] == "hipreportcheck.esp", calls
                        last_report = calls[:configs[4]].count("hipreport.esp")
                        assert set(reports[next_report:last_report]) == {original}, reports
                        final_rebind_report = calls[:configs[5]].count("hipreport.esp")
                        reports_before_rebind = reports[last_report:final_rebind_report]
                        assert reports_before_rebind[:2] == [changed, changed], reports
                        assert reports_before_rebind[-1] == original and reports_before_rebind == sorted(
                            reports_before_rebind, key=lambda report: report == original), reports
                        assert set(reports_before_rebind) == {changed, original}, reports
                        assert set(reports[final_rebind_report:]) == {original}, reports
                        print("After rebound ESP decline, reconnect and forwarding restoration each required fresh approval")
                        print("The final rebind submitted unchanged HIP before ESP traffic")
                    else:
                        assert set(reports[next_report:]) == {original}, reports
                    if declined_rebind:
                        print("Declining the changed HIP report stopped rebound ESP without uploading it")
                    elif not reconnected_after_rebind:
                        print("Restored ESP verified silent refresh, fresh approval for restored IPv6 forwarding, and rebind")
                else:
                    changed = reports[-1]
                    assert original.split()[0] != changed.split()[0], reports
                    assert reports[report_index:] == sorted(reports[report_index:],
                                                             key=lambda report: report != original), reports
                    assert set(reports) == {original, changed}, reports
                    print("Active ESP loss recovered on TLS, with silent unchanged HIP and approval for the changed report")
                return
            if args.hip in ("refresh", "posture", "tessera", "tessera-domain", "tessera-rebind"):
                connections = 2 if args.hip == "tessera-rebind" else 1
                assert calls.count("getconfig.esp") == connections, calls
                assert calls.count("login.esp") == 1, calls
                echoes = 2
                if args.hip in ("tessera-domain", "tessera-rebind"):
                    transitions = []
                    for count, digest in map(str.split, receipts.read_text().splitlines()):
                        if not transitions or transitions[-1] != (count, digest):
                            transitions.append((count, digest))
                    assert [count for count, _ in transitions] == ["0", "1", "0", "1"], transitions
                    assert transitions[0] == transitions[2] and transitions[1] == transitions[3], transitions
                    echoes = len(transitions)
                if esp:
                    assert not esp.errors, esp.errors
                    assert len(esp.echo) == echoes and len(set(esp.echo)) == connections, esp.echo
                    if connections == 2:
                        assert esp.echo[0] == esp.echo[1] and esp.echo[2] == esp.echo[3], esp.echo
                    assert "ssl-tunnel-connect.sslvpn" not in calls, calls
                else:
                    assert calls.count("ssl-tunnel-connect.sslvpn") == connections and calls.count("echo") == echoes, calls
                    if connections == 2:
                        data_calls = [call for call in calls if call in ("ssl-tunnel-connect.sslvpn", "echo")]
                        assert data_calls == ["ssl-tunnel-connect.sslvpn", "echo", "echo"] * 2, calls
                assert calls[-1] == "logout.esp", calls
                if connections == 2:
                    print("HIP approval preceded traffic on the rebound data connection")
                else:
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

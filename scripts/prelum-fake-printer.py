#!/usr/bin/env python3
"""A plain-IPP printer whose state a test sets (a test oracle, never shipped).

prelum-r5-vm.sh needs a printer that runs out of paper on cue, which
ippeveprinter can't do. This one answers what prelum-ipp asks (Get-Printer-
Attributes, Create-Job, Send-Document, Get-Job-Attributes, Cancel-Job,
Identify-Printer) over HTTP on --port, takes PDF, and reads its state from
--state each time it is asked: "idle", or "stopped REASON..." (for example
"stopped media-empty-error"). A job it takes prints (completes) a few
seconds later, but only while the state is idle; documents go to --spool.

    python3 -I prelum-fake-printer.py --port 8662 --state FILE --spool DIR
"""

import argparse
import hashlib
import http.server
import os
import ssl
import struct
import threading
import time
import uuid

A = argparse.ArgumentParser()
A.add_argument("--bind", default="0.0.0.0")
A.add_argument("--port", type=int, required=True)
A.add_argument("--state", required=True)
A.add_argument("--spool", required=True)
A.add_argument("--name", default="Fake Printer")
A.add_argument("--tls-cert")
A.add_argument("--tls-key")
A.add_argument("--uuid-override-file", help="read an alternate printer UUID from this file when present")
A.add_argument("--tls-alt-cert")
A.add_argument("--tls-alt-key")
A.add_argument("--tls-alt-flag", help="serve the alternative key while this file exists")
A.add_argument("--test-pin-job", action="append", default=[], help="title of a job that must supply the fixture PIN")
A.add_argument("--test-no-job-password-flag", help="omit job-password-supported while this file exists")
A.add_argument("--test-no-create-job-flag", help="omit Create-Job support while this file exists")
A.add_argument("--test-no-send-document-flag", help="omit Send-Document support while this file exists")
A.add_argument("--test-ignore-pin-job", help="title of a fixture PIN job to create with ignored attributes")
ARGS = A.parse_args()
if bool(ARGS.tls_cert) != bool(ARGS.tls_key):
    A.error("--tls-cert and --tls-key must be used together")
alt_tls = (ARGS.tls_alt_cert, ARGS.tls_alt_key, ARGS.tls_alt_flag)
if any(alt_tls) and not (all(alt_tls) and ARGS.tls_cert):
    A.error("alternate TLS requires a cert, key and flag in addition to the original cert and key")
UUID = "urn:uuid:" + str(uuid.uuid5(uuid.NAMESPACE_URL, "prelum-fake-%d" % ARGS.port))

# Value tags.
INT, BOOL, ENUM = 0x21, 0x22, 0x23
TEXT, NAME, KEYWORD, URI, CHARSET, LANG, MIME = 0x41, 0x42, 0x44, 0x45, 0x47, 0x48, 0x49
# Group tags.
OPG, JOBG, ENDG, PRG, UNSUPG = 0x01, 0x02, 0x03, 0x04, 0x05

LOCK = threading.Lock()
JOBS = {}  # id -> dict(state, name, done_at, impressions)
NEXT = [1]


def state():
    try:
        words = open(ARGS.state).read().split()
    except OSError:
        words = ["idle"]
    return (words[0] if words else "idle"), words[1:]


def enc(tag, name, value):
    if tag in (INT, ENUM):
        v = struct.pack(">i", value)
    elif tag == BOOL:
        v = b"\x01" if value else b"\x00"
    else:
        v = value.encode()
    n = name.encode()
    return bytes([tag]) + struct.pack(">H", len(n)) + n + struct.pack(">H", len(v)) + v


def attr(tag, name, values):
    if not isinstance(values, list):
        values = [values]
    out = enc(tag, name, values[0])
    for v in values[1:]:
        out += enc(tag, "", v)
    return out


def parse(body):
    """(op, request id, {name: [values]}, document bytes)."""
    _ver, op, rid = struct.unpack(">HHI", body[:8])
    i, attrs, last = 8, {}, None
    while i < len(body):
        t = body[i]
        i += 1
        if t == ENDG:
            break
        if t < 0x10:
            continue
        nl = struct.unpack(">H", body[i:i + 2])[0]
        name = body[i + 2:i + 2 + nl].decode(errors="replace")
        i += 2 + nl
        vl = struct.unpack(">H", body[i:i + 2])[0]
        raw = body[i + 2:i + 2 + vl]
        i += 2 + vl
        if t in (INT, ENUM) and vl == 4:
            val = struct.unpack(">i", raw)[0]
        else:
            val = raw.decode(errors="replace")
        if name:
            last = name
            attrs[name] = [val]
        elif last:
            attrs[last].append(val)
    return op, rid, attrs, body[i:]


def head(status, rid):
    return struct.pack(">HHI", 0x0200, status, rid) + bytes([OPG]) + attr(CHARSET, "attributes-charset", "utf-8") + attr(LANG, "attributes-natural-language", "en")


def printer_attrs(host):
    st, reasons = state()
    uri = "%s://%s/ipp/print" % ("ipps" if ARGS.tls_cert else "ipp", host)
    printer_uuid = UUID
    if ARGS.uuid_override_file:
        try:
            with open(ARGS.uuid_override_file, encoding="ascii") as override:
                printer_uuid = "urn:uuid:" + str(uuid.UUID(override.read().strip()))
        except FileNotFoundError:
            pass
    pin_supported = bool(ARGS.test_pin_job or ARGS.test_ignore_pin_job)
    if ARGS.test_no_job_password_flag and os.path.exists(ARGS.test_no_job_password_flag):
        pin_supported = False
        print("fake printer: withholding job-password capability", flush=True)
    operations = [0x0002, 0x0004, 0x0005, 0x0006, 0x0008, 0x0009, 0x000B, 0x003C]
    if ARGS.test_no_create_job_flag and os.path.exists(ARGS.test_no_create_job_flag):
        operations.remove(0x0005)
        print("fake printer: withholding create-job capability", flush=True)
    if ARGS.test_no_send_document_flag and os.path.exists(ARGS.test_no_send_document_flag):
        operations.remove(0x0006)
        print("fake printer: withholding send-document capability", flush=True)
    return bytes([PRG]) + b"".join([
        attr(URI, "printer-uri-supported", uri),
        attr(INT, "job-password-supported", 8) if pin_supported else b"",
        attr(KEYWORD, "uri-security-supported", "tls" if ARGS.tls_cert else "none"),
        attr(URI, "printer-uuid", printer_uuid),
        attr(TEXT, "printer-info", ARGS.name),
        attr(TEXT, "printer-make-and-model", "Prelum Fake"),
        attr(TEXT, "printer-location", "the test bench"),
        attr(ENUM, "printer-state", 5 if st == "stopped" else 3),
        attr(KEYWORD, "printer-state-reasons", reasons or ["none"]),
        attr(MIME, "document-format-supported", ["application/pdf"]),
        attr(KEYWORD, "media-supported", ["iso_a4_210x297mm", "na_letter_8.5x11in"]),
        attr(KEYWORD, "media-default", "iso_a4_210x297mm"),
        attr(KEYWORD, "sides-supported", ["one-sided"]),
        attr(KEYWORD, "print-color-mode-supported", ["color", "monochrome"]),
        attr(ENUM, "operations-supported", operations),
        attr(KEYWORD, "identify-actions-supported", ["display", "sound"]),
        attr(BOOL, "printer-is-accepting-jobs", True),
    ])


def tick():
    """Jobs move on only while the printer is idle."""
    st, _ = state()
    now = time.time()
    with LOCK:
        for jid, j in JOBS.items():
            if j["state"] == 5 and st == "idle" and j["data"]:
                if j["done_at"] is None:
                    j["done_at"] = now + 3
                elif now >= j["done_at"]:
                    j["state"], j["impressions"] = 9, 1
                    print(f"fake printer: job {jid} completed", flush=True)
            elif j["state"] == 5 and st != "idle":
                j["done_at"] = None


class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *a):
        pass

    def body(self):
        if "chunked" in (self.headers.get("Transfer-Encoding") or "").lower():
            out = b""
            while True:
                n = int(self.rfile.readline().strip().split(b";")[0], 16)
                if n == 0:
                    while self.rfile.readline() not in (b"\r\n", b"\n", b""):
                        pass
                    return out
                out += self.rfile.read(n)
                self.rfile.readline()
        return self.rfile.read(int(self.headers.get("Content-Length") or 0))

    def do_POST(self):
        op, rid, a, doc = parse(self.body())
        tick()
        host = self.headers.get("Host") or "localhost:%d" % ARGS.port
        st, reasons = state()
        jid = (a.get("job-id") or [0])[0]
        out = head(0, rid)
        name = {0x0B: "Get-Printer-Attributes", 0x05: "Create-Job", 0x06: "Send-Document", 0x09: "Get-Job-Attributes",
                0x08: "Cancel-Job", 0x3C: "Identify-Printer", 0x04: "Validate-Job", 0x02: "Print-Job"}.get(op, hex(op))
        print("fake printer: %s job %s (state %s %s)" % (name, jid, st, " ".join(reasons)), flush=True)
        if op == 0x0B:
            out += printer_attrs(host)
        elif op in (0x05, 0x02):
            title = (a.get("job-name") or ["?"])[0]
            ignore_pin = title == ARGS.test_ignore_pin_job
            pin_job = ignore_pin or title in ARGS.test_pin_job
            if pin_job and (op != 0x05 or a.get("job-password") != ["0427"]
                            or a.get("ipp-attribute-fidelity") != ["\x01"] or doc):
                self.send_error(400, "Invalid fixture PIN submission")
                return
            with LOCK:
                jid = NEXT[0]
                NEXT[0] += 1
                JOBS[jid] = {"state": 5, "done_at": None, "impressions": 0, "data": op == 0x02 and bool(doc),
                             "name": title}
            if ignore_pin:
                # successful-ok-ignored-or-substituted-attributes, with a real job ID.
                out = head(0x0001, rid)
                print(f"fake printer: ignored fixture PIN for job {jid}", flush=True)
            elif pin_job:
                print(f"fake printer: accepted fixture PIN for job {jid} before document", flush=True)
            if op == 0x02:
                open(os.path.join(ARGS.spool, "job-%d.pdf" % jid), "wb").write(doc)
            out += bytes([JOBG]) + attr(INT, "job-id", jid) + attr(URI, "job-uri", "ipp://%s/ipp/print/%d" % (host, jid)) + attr(ENUM, "job-state", 3)
        elif op == 0x06:
            if "job-password" in a:
                self.send_error(400, "PIN in document request")
                return
            with LOCK:
                j = JOBS.get(jid)
                if j:
                    j["data"] = True
            if j is None:
                out = struct.pack(">HHI", 0x0200, 0x0406, rid) + out[8:]
            else:
                open(os.path.join(ARGS.spool, "job-%d.pdf" % jid), "wb").write(doc)
                digest = hashlib.sha256(doc).hexdigest()
                document_format = (a.get("document-format") or ["missing"])[0]
                tls = self.connection.version() if isinstance(self.connection, ssl.SSLSocket) else "none"
                print(
                    f"fake printer: got job {jid} ({len(doc)} bytes) {j['name']} "
                    f"sha256={digest} format={document_format} tls={tls}",
                    flush=True,
                )
                out += bytes([JOBG]) + attr(INT, "job-id", jid) + attr(ENUM, "job-state", 5)
        elif op == 0x09:
            with LOCK:
                j = JOBS.get(jid)
            if j is None:
                out = struct.pack(">HHI", 0x0200, 0x0406, rid) + out[8:]
            else:
                js = j["state"]
                if js == 5 and st == "stopped":
                    js, jr = 6, ["printer-stopped"]
                else:
                    jr = {9: ["job-completed-successfully"], 7: ["job-canceled-by-user"]}.get(js, ["job-printing"])
                out += bytes([JOBG]) + attr(INT, "job-id", jid) + attr(ENUM, "job-state", js) + attr(KEYWORD, "job-state-reasons", jr) + attr(INT, "job-impressions-completed", j["impressions"])
        elif op == 0x08:
            with LOCK:
                if jid in JOBS:
                    JOBS[jid]["state"] = 7
                    print(f"fake printer: job {jid} canceled", flush=True)
        out += bytes([ENDG])
        self.send_response(200)
        self.send_header("Content-Type", "application/ipp")
        self.send_header("Content-Length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)


os.makedirs(ARGS.spool, exist_ok=True)
http.server.ThreadingHTTPServer.allow_reuse_address = True
srv = http.server.ThreadingHTTPServer((ARGS.bind, ARGS.port), H)
if ARGS.tls_cert:
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(ARGS.tls_cert, ARGS.tls_key)
    if ARGS.tls_alt_cert:
        alternate = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        alternate.load_cert_chain(ARGS.tls_alt_cert, ARGS.tls_alt_key)

        def select_key(connection, _name, _context):
            # The callback also runs for IP clients that send no SNI.
            if os.path.exists(ARGS.tls_alt_flag):
                connection.context = alternate

        context.sni_callback = select_key
    srv.socket = context.wrap_socket(srv.socket, server_side=True)
print("fake printer: listening on %d (%s)" % (ARGS.port, UUID), flush=True)
srv.serve_forever()

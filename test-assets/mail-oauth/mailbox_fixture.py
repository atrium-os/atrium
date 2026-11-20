"""Single-message IMAP mailbox and SMTP sink for the Mail window gate."""
import json
import re

HEADERS = (
    "From: Robin Vale <robin@school.test>\r\nTo: Ada <ada@outlook.com>\r\n"
    "Subject: Greenhouse visit\r\nDate: Fri, 20 Nov 2026 08:00:00 +0000\r\n"
    "Message-ID: <greenhouse-visit@school.test>\r\n"
    "Content-Type: text/plain; charset=utf-8\r\n\r\n"
).encode("ascii")
MESSAGE = HEADERS + b"Please bring the blue notebook.\r\n"


def inbox(peer, renewed=False):
    messages = {41: (HEADERS, MESSAGE)}
    if renewed:
        new = HEADERS.replace(b"Greenhouse visit", b"Greenhouse reopened").replace(b"greenhouse-visit@", b"greenhouse-reopened@")
        messages[42] = (new, new + b"The second tray is ready.\r\n")
    for _ in range(256):
        tag, command = peer.line().split(" ", 1)
        words = command.upper().split()
        if words[0] == "LIST":
            peer.put('* LIST (\\HasNoChildren) "/" "INBOX"')
        elif words[0] in ("SELECT", "EXAMINE") and words[1].strip('"') == "INBOX":
            peer.put(f"* {len(messages)} EXISTS")
            peer.put("* OK [UIDVALIDITY 11] stable")
            peer.put(f"* OK [UIDNEXT {max(messages) + 1}] next")
        elif words[:2] == ["UID", "SEARCH"]:
            peer.put("* SEARCH " + " ".join(map(str, messages)))
        elif words[:2] == ["UID", "FETCH"]:
            section = next((s for s in ("HEADER", "") if "BODY.PEEK[" + s + "]" in command.upper()), None)
            for seq, (uid, (header, message)) in enumerate(messages.items(), 1):
                if words[2] != "1:*" and str(uid) not in words[2].split(","):
                    continue
                if section is None:
                    peer.put(f"* {seq} FETCH (UID {uid} FLAGS ())")
                else:
                    data = header if section else message
                    peer.put(f'* {seq} FETCH (UID {uid} FLAGS () RFC822.SIZE {len(message)} INTERNALDATE "20-Nov-2026 08:00:00 +0000" BODY[{section}] {{{len(data)}}}')
                    peer.sock.sendall(data + b")\r\n")
                    print("IMAP supplied", "headers" if section else "message", uid, flush=True)
        elif words[0] == "LOGOUT":
            peer.put("* BYE")
            peer.put(tag + " OK logout")
            return
        elif words[0] != "NOOP" and words[:2] != ["UID", "STORE"]:
            peer.put(tag + " BAD unsupported fixture command")
            print("IMAP unsupported", words[:2], flush=True)
            continue
        peer.put(tag + " OK done")


def submission(peer, directory):
    sender = None
    recipients = []
    for _ in range(32):
        line = peer.line()
        addr = re.fullmatch(r"(MAIL FROM|RCPT TO):\s*<([^<>\s]+)>(?: SIZE=\d+)?", line, re.I)
        if addr and addr[1].upper() == "MAIL FROM":
            sender, recipients = addr[2], []
            peer.put("250 sender ok")
        elif addr and sender and addr[1].upper() == "RCPT TO":
            recipients.append(addr[2])
            peer.put("250 recipient ok")
        elif line == "DATA" and sender and recipients:
            peer.put("354 send data")
            message = bytearray()
            while len(message) <= 1 << 20:
                data = peer.reader.readline(1001)
                if not data.endswith(b"\r\n") or len(data) > 1000:
                    raise ValueError("bad SMTP data line")
                if data == b".\r\n":
                    break
                message.extend(data[1:] if data.startswith(b"..") else data)
            else:
                raise ValueError("oversize fixture submission")
            # Keep every accepted envelope and its exact, unstuffed MIME bytes.
            import tempfile
            from pathlib import Path
            capture = Path(tempfile.mkdtemp(prefix="received-", dir=directory))
            (capture / "message.eml").write_bytes(message)
            (capture / "envelope.json").write_text(json.dumps(dict(sender=sender, recipients=recipients)))
            peer.put("250 accepted")
            print("SMTP accepted DATA", flush=True)
            sender, recipients = None, []
        elif line == "QUIT":
            peer.put("221 bye")
            return
        else:
            peer.put("503 unexpected command")


def verify(directory, sender, recipient, subject, body):
    """Check the delivered envelope independently of MIME display headers."""
    from email import policy
    from email.parser import BytesParser
    from email.utils import getaddresses

    captures = list(directory.glob("received-*"))
    if len(captures) != 1:
        raise ValueError(f"expected one SMTP delivery, found {len(captures)}")
    capture = captures[0]
    envelope = json.loads((capture / "envelope.json").read_text())
    if envelope != {"sender": sender, "recipients": [recipient]}:
        raise ValueError("SMTP envelope mismatch")
    message = BytesParser(policy=policy.default).parsebytes((capture / "message.eml").read_bytes())
    for header, expected in [("From", sender), ("To", recipient)]:
        if [addr for _, addr in getaddresses(message.get_all(header, []))] != [expected]:
            raise ValueError(f"MIME {header} mismatch")
    if message["Subject"] != subject:
        raise ValueError("MIME subject mismatch")
    plain = message.get_body(preferencelist=("plain",))
    if plain is None or plain.get_content().removesuffix("\r\n").removesuffix("\n") != body:
        raise ValueError("decoded text/plain body mismatch")
    print("One delivery. SMTP sender and recipient, MIME From, To, Subject and decoded body all match.")


if __name__ == "__main__":
    import sys
    from pathlib import Path
    verify(Path(sys.argv[1]), *sys.argv[2:])

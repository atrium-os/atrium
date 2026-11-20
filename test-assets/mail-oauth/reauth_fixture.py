"""Delegate interactive sign-in to the existing loopback PKCE provider."""
import http.client
import json
import socket
import ssl

HOST = "login.microsoftonline.com"


def callback(peer, directory, request, headers, body, issue_grant):
    # The courier sees the rotation fixture's TLS endpoint. The upstream
    # independently validates the flow cookie, state, code and PKCE verifier.
    method, path, version = request.split(" ")
    if method not in ("GET", "POST") or version != "HTTP/1.1" or not path.startswith("/"):
        raise ValueError("invalid fixture request")
    port = int((directory / "callback.port").read_text())
    ctx = ssl.create_default_context(cafile=str(directory / "callback-ca.pem"))
    with socket.create_connection(("127.0.0.1", port), timeout=10) as tcp:
        with ctx.wrap_socket(tcp, server_hostname=HOST) as tls:
            head = [request, "Host: " + HOST]
            if "cookie" in headers:
                head.append("Cookie: " + headers["cookie"])
            head.append("Content-Length: " + str(len(body)))
            tls.sendall(("\r\n".join(head) + "\r\n\r\n").encode("ascii") + body)
            r = http.client.HTTPResponse(tls)
            r.begin()
            data = r.read(65537)
            if len(data) > 65536:
                raise ValueError("oversize fixture reply")
            if method == "POST" and r.status == 200:
                fields = json.loads(data)
                fields["refresh_token"] = issue_grant()
                data = json.dumps(fields).encode()
            peer.put(f"HTTP/1.1 {r.status} {r.reason}")
            for k, v in r.getheaders():
                if k.lower() in ("location", "set-cookie", "content-type"):
                    peer.put(f"{k}: {v}")
            peer.put("Content-Length: " + str(len(data)))
            peer.put("Connection: close")
            peer.put("")
            peer.sock.sendall(data)

#!/usr/bin/env python3
"""Loopback TLS packet, rebind and logout check for the Crypta host harness.

Build atrium-crypta-ac and the ac_harness example in the Crypta workspace first.
"""

import os
from pathlib import Path
import socket
import ssl
import struct
import subprocess
import tempfile
import threading
from urllib.parse import parse_qs, urlsplit

root = Path(__file__).resolve().parents[1] / "crypta/target/debug"
with tempfile.TemporaryDirectory() as tmp:
    cert, key = f'{tmp}/cert.pem', f'{tmp}/key.pem'
    conf = f'{tmp}/openssl.cnf'
    with open(conf, 'w') as out:
        out.write('[req]\ndistinguished_name=dn\nx509_extensions=v3\n[dn]\n[v3]\nsubjectAltName=DNS:localhost\n')
    subprocess.run(['openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1',
                    '-config', conf, '-subj', '/CN=localhost', '-keyout', key, '-out', cert], check=True, capture_output=True)
    hashes = subprocess.check_output(['openssl', 'x509', '-in', cert, '-noout', '-fingerprint', '-sha256'])
    pin = hashes.decode().split('=')[1].replace(':', '').strip()
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(cert, key)
    listener = socket.create_server(('127.0.0.1', 0))
    listener.settimeout(15)
    calls = []
    errors = []
    def serve():
        try:
            while True:
                raw, _ = listener.accept()
                with ctx.wrap_socket(raw, server_side=True) as stream:
                    stream.settimeout(10)
                    with stream.makefile('rb') as reader:
                        first = reader.readline().decode().split()
                        if not first:
                            continue
                        method, url, _ = first
                        parsed = urlsplit(url)
                        path = parsed.path.rsplit('/', 1)[-1]
                        calls.append(path)
                        headers = {}
                        while (line := reader.readline()) != b'\r\n':
                            name, value = line.decode().split(':', 1)
                            headers[name.lower()] = value.strip()
                        fields = parse_qs(reader.read(int(headers.get('content-length', '0'))).decode() or parsed.query)
                        if method == 'POST':
                            assert headers['user-agent'] == 'PAN GlobalProtect'
                        if path == 'prelogin.esp':
                            assert fields['clientos'] == ['Linux']
                            body = '<prelogin-response><status>Success</status></prelogin-response>'
                        elif path == 'login.esp':
                            assert fields['user'] == ['alice'] and fields['passwd'] == ['a&+ b']
                            args = ['', 'c+&amp;', '', 'office', 'alice', 'ldap', 'vsys', 'corp', '', '', 'tunnel', '-1', '4100']
                            body = '<jnlp><application-desc>' + ''.join(f'<argument>{a}</argument>' for a in args) + '</application-desc></jnlp>'
                        elif path == 'getconfig.esp':
                            assert fields['authcookie'] == ['c+&'] and fields['portal'] == ['office']
                            body = '<response status="success"><ssl-tunnel-url>/ssl-tunnel-connect.sslvpn</ssl-tunnel-url><ip-address>10.77.0.2</ip-address><netmask>255.255.255.0</netmask><access-routes><member>10.77.0.0/24</member></access-routes></response>'
                        elif path == 'hipreportcheck.esp':
                            assert fields['client-ip'] == ['10.77.0.2']
                            body = '<response status="success"><hip-report-needed>no</hip-report-needed></response>'
                        elif path == 'ssl-tunnel-connect.sslvpn':
                            assert fields == {'user': ['alice'], 'authcookie': ['c+&']}
                            stream.sendall(b'START_')
                            stream.sendall(b'TUNNEL')
                            header = reader.read(16)
                            assert header[:6] == bytes.fromhex('1a2b3c4d0800')
                            packet = bytearray(reader.read(struct.unpack('!H', header[6:8])[0]))
                            assert packet[12:20] == bytes([10,77,0,2,10,77,0,1]) and packet[20] == 8
                            packet[12:20] = bytes([10,77,0,1,10,77,0,2])
                            packet[20] = 0
                            stream.sendall(header[:9])
                            stream.sendall(header[9:] + packet)
                            assert reader.read(1) == b""
                            continue
                        elif path == 'logout.esp':
                            assert fields['authcookie'] == ['c+&']
                            body = '<response status="success"/>'
                        else:
                            raise AssertionError(path)
                        stream.sendall(f'HTTP/1.1 200 OK\r\nContent-Length: {len(body)}\r\nConnection: close\r\n\r\n{body}'.encode())
                        if path == 'logout.esp':
                            return
        except Exception as e:
            errors.append(e)
    t = threading.Thread(target=serve, daemon=True)
    t.start()
    result = subprocess.run([f'{root}/examples/ac_harness', f'{root}/atrium-crypta-ac', 'localhost',
                             f'127.0.0.1:{listener.getsockname()[1]}', cert, '--gp', '--pin', pin, '--ping',
                             '10.77.0.1', 'username=alice', 'password=@held:a&+ b'],
                            env={**os.environ, 'HARNESS_REBIND': '1'}, check=True, timeout=25, capture_output=True, text=True)
    t.join(1)
    print(result.stdout, result.stderr, sep='')
    assert not errors, errors
    assert 'harness: ok' in result.stdout
    assert result.stdout.count('echo reply from 10.77.0.1') == 2
    assert calls == ['prelogin.esp', 'login.esp'] + ['getconfig.esp', 'hipreportcheck.esp', 'ssl-tunnel-connect.sslvpn'] * 2 + ['logout.esp'], calls
    print("GlobalProtect TLS packet check passed")

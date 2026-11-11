# Runs cdmhost in H0b mode and relays its licence request to a test licence server.
# Usage: licence_driver.py CDMHOST LIB PSSH_B64 DIR URL
import subprocess, sys, os, time, urllib.request
host, lib, pssh, d, url = sys.argv[1:6]
for f in ('msg-licreq.bin', 'resp.bin', 'clear.h264'):
    try: os.remove(os.path.join(d, f))
    except FileNotFoundError: pass
p = subprocess.Popen([host, lib, pssh, d])
req = os.path.join(d, 'msg-licreq.bin')
for _ in range(300):
    if os.path.exists(req) and os.path.getsize(req) > 0: break
    time.sleep(0.1)
time.sleep(0.2)
body = open(req, 'rb').read()
print('driver: POST', len(body), 'bytes to', url, flush=True)
r = urllib.request.urlopen(urllib.request.Request(url, data=body, headers={'Content-Type': 'application/octet-stream'}), timeout=30)
resp = r.read(); print('driver: HTTP', r.status, len(resp), 'bytes', flush=True)
open(os.path.join(d, 'resp.bin.tmp'), 'wb').write(resp); os.rename(os.path.join(d, 'resp.bin.tmp'), os.path.join(d, 'resp.bin'))
sys.exit(p.wait())

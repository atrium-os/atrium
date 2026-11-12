#!/usr/bin/env python3
"""Run commands on the KVM test-boot guest's serial console (root, no password).

    python3 -I scripts/r9700/testboot-console.py 'cmd1' 'cmd2' ...

Restarts the test-boot VM from ~/r9700-testboot (made by build-on-box.sh
testboot) with its serial port on a unix socket, waits for login, logs in as
root and prints each command's output. For when ssh into the guest fails.
"""
import os, socket, subprocess, sys, time

T = os.path.expanduser("~/r9700-testboot")
SOCK = os.path.join(T, "serial.sock")

def sh(cmd):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True)

pid = os.path.join(T, "qemu.pid")
if os.path.exists(pid):
    sh(f"kill $(cat {pid}) 2>/dev/null; sleep 2")
sh(f"pkill -f 'file={T}/disk.img' ; sleep 1; rm -f {SOCK}")
subprocess.run([
    "qemu-system-x86_64", "-enable-kvm", "-machine", "q35", "-cpu", "host", "-smp", "4", "-m", "4G",
    "-display", "none",
    "-drive", "if=pflash,format=raw,readonly=on,file=/usr/share/OVMF/OVMF_CODE_4M.fd",
    "-drive", f"if=pflash,format=raw,file={T}/vars.fd",
    "-drive", f"file={T}/disk.img,format=raw,if=none,id=d0", "-device", "nvme,drive=d0,serial=testboot",
    "-netdev", "user,id=n0,hostfwd=tcp:127.0.0.1:2224-:22",
    "-device", "e1000,netdev=n0,mac=52:54:00:12:34:56",
    "-chardev", f"socket,id=s0,path={SOCK},server=on,wait=off,logfile={T}/serial-console.log",
    "-serial", "chardev:s0", "-daemonize", "-pidfile", pid], check=True)

s = None
for _ in range(60):
    try:
        s = socket.socket(socket.AF_UNIX); s.connect(SOCK); break
    except OSError:
        time.sleep(1)
s.settimeout(1)

def read_for(sec):
    out = b""; end = time.time() + sec
    while time.time() < end:
        try:
            d = s.recv(65536)
            if d: out += d
        except socket.timeout:
            pass
    return out.decode(errors="replace")

buf = ""
deadline = time.time() + 600
while "login:" not in buf and time.time() < deadline:
    buf += read_for(5)
if "login:" not in buf:
    print(buf[-3000:]); sys.exit("no login prompt in 10 min")
s.sendall(b"root\r"); read_for(4)
s.sendall(b"stty cols 200; export TERM=dumb PS1='# '\r"); read_for(2)
for c in sys.argv[1:]:
    s.sendall(c.encode() + b"; echo __END__\r")
    out = ""; end = time.time() + 120
    while out.count("__END__") < 2 and time.time() < end:
        out += read_for(2)
    print(f"### {c}\n{out}")

#!/usr/bin/env python3
"""Run the hardware-free M9 gate from first boot through poweroff."""
import argparse
import os
from pathlib import Path
import re
import shutil
import signal
import socket
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]


def provision(console, public_key):
    """Commission SSH over the stock image's serial login, then enable rctl."""
    pending = b""

    def wait(pattern, seconds):
        nonlocal pending
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            match = re.search(pattern, pending)
            if match:
                pending = pending[match.end():]
                return
            try:
                chunk = console.recv(65536)
            except socket.timeout:
                continue
            if not chunk:
                raise RuntimeError("guest serial console closed")
            pending += chunk
            if b"\x1b[6n" in pending:
                console.sendall(b"\x1b[24;80R")
                pending = pending.replace(b"\x1b[6n", b"")
        raise TimeoutError(f"serial console waiting for {pattern!r}")

    def send(line):
        console.sendall(line.encode() + b"\r")

    console.settimeout(1)
    send("")
    wait(rb"login: ", 600)
    send("root")
    wait(rb"# ", 45)
    # Short writes avoid the stock console's long-line editing/redraw.
    for line in [
        "install -d -m 0700 /root/.ssh",
        f"printf '%s' '{public_key[:40]}' > /root/.ssh/authorized_keys",
        f"printf '%s\\n' '{public_key[40:]}' >> /root/.ssh/authorized_keys",
        "chmod 0600 /root/.ssh/authorized_keys",
        "sysrc sshd_enable=YES",
        "echo 'PermitRootLogin prohibit-password' >> /etc/ssh/sshd_config",
        "echo kern.racct.enable=1 >> /boot/loader.conf",
    ]:
        send(line)
        wait(rb"# ", 60)
    send("shutdown -r now")


def firmware(qemu):
    for code, variables in [
        (qemu.parent.parent / "share/qemu/edk2-x86_64-code.fd",
         qemu.parent.parent / "share/qemu/edk2-i386-vars.fd"),
        (Path("/usr/share/OVMF/OVMF_CODE_4M.fd"), Path("/usr/share/OVMF/OVMF_VARS_4M.fd")),
    ]:
        if code.is_file() and variables.is_file():
            return code, variables
    raise RuntimeError("amd64 QEMU UEFI firmware and variable template missing")


def boot_and_run(cache, image, out):
    qemu = shutil.which("qemu-system-x86_64")
    if not qemu or not (cache / "sysroot/usr/lib/Scrt1.o").is_file():
        raise RuntimeError("install QEMU and supply the matching amd64 sysroot under VMDIR")
    code, variables = firmware(Path(qemu).resolve())
    # Allocate fresh writable state. Leave the input image and sysroot untouched.
    with (out / "vm.qcow2").open("xb") as disk:
        subprocess.run(["xz", "-dc", image], stdout=disk, check=True)
    subprocess.run(["qemu-img", "resize", "vm.qcow2", "+20G"], cwd=out, check=True)
    shutil.copyfile(variables, out / "vars.fd")
    key = out / "guest-key"
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", key], check=True)
    public_key = " ".join(key.with_suffix(".pub").read_text().split()[:2])
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = str(listener.getsockname()[1])
    ssh = ["ssh", "-i", key, "-p", port, "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes",
           "-o", "ConnectTimeout=4", "-o", "StrictHostKeyChecking=accept-new",
           "-o", f"UserKnownHostsFile={out / 'known_hosts'}", "root@127.0.0.1"]
    command = [qemu, "-machine", "q35", "-accel", "tcg,thread=multi", "-cpu", "Haswell-noTSX",
               "-smp", "4", "-m", "4096", "-display", "none",
               "-drive", f"if=pflash,format=raw,unit=0,file={code},readonly=on",
               "-drive", "if=pflash,format=raw,unit=1,file=vars.fd",
               "-drive", "file=vm.qcow2,format=qcow2,if=virtio",
               "-netdev", f"user,id=m9,hostfwd=tcp:127.0.0.1:{port}-:22",
               "-device", "virtio-net-pci,netdev=m9",
               "-chardev", "socket,id=console,path=serial.sock,server=on,wait=off,logfile=serial.log",
               "-serial", "chardev:console", "-monitor", "unix:monitor.sock,server=on,wait=off"]
    with (out / "qemu.log").open("w") as log:
        vm = subprocess.Popen(command, cwd=out, stdout=log, stderr=log, start_new_session=True)
    ready = False
    try:
        for _ in range(100):
            if vm.poll() is not None:
                raise RuntimeError("QEMU exited during startup; see qemu.log")
            if (out / "serial.sock").is_socket():
                break
            time.sleep(0.1)
        with socket.socket(socket.AF_UNIX) as console:
            console.connect(str(out / "serial.sock"))
            provision(console, public_key)
        deadline = time.monotonic() + 600
        while time.monotonic() < deadline and vm.poll() is None:
            try:
                status = subprocess.run(ssh + ["test \"$(sysctl -n kern.racct.enable)\" = 1"],
                                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=15)
            except subprocess.TimeoutExpired:
                continue
            if status.returncode == 0:
                ready = True
                break
            time.sleep(2)
        if not ready:
            raise RuntimeError("guest did not reach SSH with resource accounting enabled")
        preflight = subprocess.run(ssh + ["""set -eu
            test "$(uname -sm)" = 'FreeBSD amd64'
            if command -v git; then exit 1; fi
            usbconfig list >/tmp/m9-usb
            if grep -Ei 'yubico|yubikey|fido' /tmp/m9-usb; then exit 1; fi
            for dev in /dev/u2f/*; do test ! -c "$dev"; done
            echo 'FreeBSD amd64: Git absent, no YubiKey or U2F device'
            """], check=True, timeout=30, stdout=subprocess.PIPE, text=True)
        print(preflight.stdout, end="", flush=True)
        env = dict(os.environ, VMDIR=str(cache), SSH_AGENT_GATE_PORT=port, SSH_AGENT_GATE_KEY=str(key))
        print("Running M9. Follow", out / "gate.log", flush=True)
        with (out / "gate.log").open("w") as log:
            log.write(preflight.stdout)
            log.flush()
            gate = subprocess.Popen(["sh", ROOT / "scripts/vm-m9-curia-check.sh", out / "screenshots"],
                                    env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            try:
                status = gate.wait(timeout=3600)
            finally:
                if gate.poll() is None:
                    os.killpg(gate.pid, signal.SIGTERM)
                    gate.wait(timeout=15)
            if status:
                raise RuntimeError(f"M9 exited with status {status}; see {out}/gate.log")
        print((out / "gate.log").read_text().splitlines()[-1], flush=True)
    finally:
        if vm.poll() is None:
            ssh_shutdown_sent = False
            if ready:
                try:
                    result = subprocess.run(ssh + ["sync; shutdown -p now"], timeout=20)
                    ssh_shutdown_sent = result.returncode == 0
                except subprocess.TimeoutExpired:
                    pass
            if not ssh_shutdown_sent and vm.poll() is None:
                with socket.socket(socket.AF_UNIX) as monitor:
                    monitor.settimeout(5)
                    monitor.connect(str(out / "monitor.sock"))
                    monitor.sendall(b"system_powerdown\n")
            try:
                vm.wait(timeout=120)
            except subprocess.TimeoutExpired:
                raise RuntimeError(f"poweroff timed out; QEMU pid {vm.pid}, disk and key retained in {out}")
        if vm.returncode != 0:
            raise RuntimeError(f"QEMU exited with status {vm.returncode}; see {out}/qemu.log")
        print("Dedicated guest powered off.", flush=True)
        for name in ("guest-key", "guest-key.pub", "vm.qcow2", "vars.fd"):
            (out / name).unlink()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cache", type=Path, default=Path(os.getenv("VMDIR", "~/atrium-vm-amd64")),
                        help="directory containing the matching sysroot and stock FreeBSD amd64 ZFS .qcow2.xz")
    parser.add_argument("--image", type=Path, help="select the stock .qcow2.xz when the cache has multiple snapshots")
    args = parser.parse_args()
    cache = args.cache.expanduser().resolve()
    image = args.image
    if image is None:
        images = list(cache.glob("FreeBSD-*-amd64-zfs-*.qcow2.xz"))
        if len(images) != 1:
            parser.error("set --image to the stock FreeBSD amd64 ZFS .qcow2.xz matching the sysroot")
        image = images[0]
    if not image.is_file():
        parser.error(f"missing snapshot: {image}")
    out = Path(tempfile.mkdtemp(prefix="m9-fresh-", dir="/tmp"))
    print(f"Snapshot: {image.name}\nRun directory: {out}", flush=True)
    boot_and_run(cache, image, out)


if __name__ == "__main__":
    def interrupt(signum, frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupt)
    main()

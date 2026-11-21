#!/usr/bin/env python3
"""Run the hardware-free M9 gate from first boot through poweroff."""
import argparse
import fcntl
import hashlib
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
SNAPSHOTS = "https://download.freebsd.org/snapshots/VM-IMAGES/16.0-CURRENT/amd64"
STOCK_IMAGE = re.compile(r"FreeBSD-16\.0-CURRENT-amd64-zfs-(?P<build>(?P<date>\d{8})-[0-9a-f]+-\d+)\.qcow2\.xz")


def fetch(url, destination=None):
    cmd = ["curl", "--fail", "--silent", "--show-error", "--location", "--proto", "=https",
           "--proto-redir", "=https", "--connect-timeout", "20", "--max-time", "1200",
           "--retry", "3", url]
    if destination is None:
        return subprocess.check_output(cmd, text=True)
    subprocess.run(cmd, stdout=destination, check=True)


def image_digest(path):
    with path.open("rb") as source:
        digest = hashlib.sha256()
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def latest_image_name():
    dates = re.findall(r'href="(\d{8})/"', fetch(f"{SNAPSHOTS}/"))
    if not dates:
        raise ValueError("no dated FreeBSD amd64 snapshots in the official index")
    date = max(dates)
    names = re.findall(r'href="([^"]+)"', fetch(f"{SNAPSHOTS}/{date}/"))
    images = [match[0] for match in map(STOCK_IMAGE.fullmatch, names) if match and match["date"] == date]
    if len(images) != 1:
        raise ValueError(f"expected one stock amd64 ZFS qcow2 snapshot for {date}")
    return images[0]


def prepare_image(cache, image):
    # Serialize discovery and publication so parallel first runs share a download.
    with (cache / ".m9-image.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if image is None:
            images = list(cache.glob("FreeBSD-*-amd64-zfs-*.qcow2.xz"))
            if len(images) > 1:
                raise ValueError("select one of the cached snapshots with --image")
            if images:
                image = images[0]
            else:
                image = cache / latest_image_name()
        elif not image.is_file():
            raise FileNotFoundError(image)

        stock = STOCK_IMAGE.fullmatch(image.name)
        if not stock:  # Explicit, locally supplied image from a different build.
            return image, image_digest(image)
        url = f"{SNAPSHOTS}/{stock['date']}"
        manifest = cache / f"CHECKSUM.SHA256-{stock['build']}"
        checksums = manifest.read_text() if manifest.is_file() else fetch(f"{url}/{manifest.name}")
        entry = re.search(rf"^SHA256 \({re.escape(image.name)}\) = ([0-9a-f]{{64}})$", checksums, re.M)
        if not entry:
            raise ValueError(f"missing exact SHA256 entry for {image.name}")
        expected = entry[1]
        if image.is_file():
            if image_digest(image) != expected:
                raise ValueError(f"SHA256 mismatch for {image}")
        else:
            print(f"Downloading {url}/{image.name}", flush=True)
            with tempfile.TemporaryDirectory(dir=cache) as stage:
                partial = Path(stage) / image.name
                with partial.open("wb") as output:
                    fetch(f"{url}/{image.name}", output)
                if image_digest(partial) != expected:
                    raise ValueError(f"downloaded {image.name} failed SHA256 verification")
                partial.replace(image)
        if not manifest.exists():
            with tempfile.TemporaryDirectory(dir=cache) as stage:
                partial = Path(stage) / manifest.name
                partial.write_text(checksums)
                partial.replace(manifest)
        print(f"Verified {image.name} SHA256 {expected}", flush=True)
        return image, expected


def prepare_sysroot(cache, digest, ssh):
    # VMDIR is specific to the image bytes, even when --image changes.
    vmdir = cache / "m9-sysroots" / digest
    vmdir.mkdir(parents=True, exist_ok=True)
    root = vmdir / "sysroot"
    with (vmdir / ".lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if not root.exists():
            print("Preparing the cross sysroot from the clean guest", flush=True)
            with tempfile.TemporaryDirectory(dir=vmdir) as stage:
                stage = Path(stage)
                archive = stage / "sysroot.tar"
                with archive.open("wb") as output:
                    subprocess.run(ssh + ["tar -C / --exclude usr/lib/debug -cf - lib usr/lib usr/include"],
                                   stdout=output, check=True, timeout=900)
                extracted = stage / "sysroot"
                extracted.mkdir()
                subprocess.run(["tar", "-xf", archive, "-C", extracted], check=True)
                for member in ("usr/lib/Scrt1.o", "usr/lib/crti.o", "usr/lib/libc.so", "usr/include/sys/types.h"):
                    if not (extracted / member).is_file():
                        raise ValueError(f"guest sysroot missing {member}")
                extracted.rename(root)
    return vmdir


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


def boot_and_run(cache, image, digest, out):
    qemu = shutil.which("qemu-system-x86_64")
    if not qemu:
        raise RuntimeError("install qemu-system-x86_64")
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
        vmdir = prepare_sysroot(cache, digest, ssh)
        env = dict(os.environ, VMDIR=str(vmdir), SSH_AGENT_GATE_PORT=port, SSH_AGENT_GATE_KEY=str(key))
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
        subprocess.run(["python3", ROOT / "scripts/disposable-vm-cleanup.py", out, "vm.qcow2"],
                       check=True)
        for name in ("guest-key", "guest-key.pub", "vars.fd"):
            if name == "vars.fd" and (out / "vm.qcow2").exists():
                continue
            (out / name).unlink()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cache", type=Path, default=Path(os.getenv("VMDIR", "~/atrium-vm-amd64")),
                        help="snapshot and sysroot cache (created on first run)")
    parser.add_argument("--image", type=Path, help="select the stock .qcow2.xz when the cache has multiple snapshots")
    args = parser.parse_args()
    cache = args.cache.expanduser().resolve()
    cache.mkdir(parents=True, exist_ok=True)
    image, digest = prepare_image(cache, args.image)
    out = Path(tempfile.mkdtemp(prefix="m9-fresh-", dir="/tmp"))
    print(f"Snapshot: {image.name}\nRun directory: {out}", flush=True)
    boot_and_run(cache, image, digest, out)


if __name__ == "__main__":
    def interrupt(signum, frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupt)
    main()

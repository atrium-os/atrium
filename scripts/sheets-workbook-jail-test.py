#!/usr/bin/env python3
"""Run as root in an isolated FreeBSD VM with the cross-built Sheets test binary.

On the host, build with cargo test --manifest-path atrium-sheets/Cargo.toml
--target aarch64-unknown-freebsd --bin atrium-sheets --no-run. Copy the
printed test binary and this script to the VM with scp.
"""

from array import array
from concurrent.futures import ThreadPoolExecutor
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile

def run(*args, **kw):
    return subprocess.check_output(args, text=True, **kw)

def main(binary):
    assert os.geteuid() == 0, "run the gate as root in a disposable FreeBSD guest"

    names = ["first.csv", "second.csv"]
    entries = []
    with tempfile.TemporaryDirectory(prefix="sheets-jails-") as temp:
        parent = Path(temp)
        parent.chmod(0o755)
        staged = parent / "bin"
        staged.mkdir(mode=0o755)
        run("install", "-m", "755", str(binary), str(staged / "sheets-test"))
        try:
            for i, name in enumerate(names):
                jail = f"workbook-test-{os.getpid()}-{i}"
                user = f"wbs{os.getpid()}_{i}"
                run("pw", "useradd", user, "-d", "/")
                root = parent / jail
                root.mkdir()
                run("mount", "-t", "tmpfs", "-o", "size=128m", "tmpfs", str(root))
                mounts = []
                entries.append((jail, user, root, mounts))
                for directory in ("lib", "libexec", "usr/lib"):
                    (root / directory).mkdir(parents=True)
                    run("mount", "-t", "nullfs", "-o", "ro", f"/{directory}", str(root / directory))
                    mounts.append(root / directory)
                (root / "bin").mkdir()
                run("mount", "-t", "nullfs", "-o", "ro", str(staged), str(root / "bin"))
                mounts.append(root / "bin")
                source = parent / name
                source.write_bytes(f"{i+1}\n".encode())
                source.chmod(0o600)

            def launch(i):
                name = names[i]
                jail, user, root, _ = entries[i]
                sender, receiver = socket.socketpair()
                try:
                    with (parent / name).open("r+b") as file:
                        body = b"OPEN\0text/csv\0w\0" + name.encode()
                        sender.sendmsg([len(body).to_bytes(2,"little"), body],
                                       [(socket.SOL_SOCKET, socket.SCM_RIGHTS, array("i", [file.fileno()]))])
                    sender.close()
                    return run("jail", "-q", "-c", f"name={jail}", f"path={root}", "persist",
                               "ip4=disable", "ip6=disable", "exec.clean", "exec.system_jail_user",
                               f"exec.jail_user={user}",
                               "exec.start=/bin/sheets-test --ignored --exact document_tests::workbook_jail_descriptor --nocapture",
                               stdin=receiver, stderr=subprocess.STDOUT, timeout=25)
                finally:
                    receiver.close()
                    sender.close()

            for stage in ("saved", "reopened"):
                with ThreadPoolExecutor(max_workers=2) as executor:
                    for name, output in zip(names, executor.map(launch, range(2))):
                        print(output, end="")
                        assert f"{stage} {name}" in output, (name, output)
                ids = [run("jls", "-j", name, "jid").strip() for name, *_ in entries]
                assert all(s.isdecimal() and int(s) for s in ids) and len(set(ids)) == 2, ids
                print("two active jails:", ", ".join(ids))
                for jail, *_ in entries:
                    run("jail", "-q", "-r", jail)
        finally:
            for jail, user, root, mounts in reversed(entries):
                subprocess.run(["jail", "-q", "-r", jail], capture_output=True)
                for mount in reversed(mounts):
                    run("umount", str(mount))
                run("umount", str(root))
                run("pw", "userdel", user)

if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(f"usage: {sys.argv[0]} SHEETS_TEST_BINARY")
    main(Path(sys.argv[1]).resolve(strict=True))

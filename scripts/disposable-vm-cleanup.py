#!/usr/bin/env python3
"""Remove explicitly owned QCOW2 disks after the runner has reaped QEMU."""
import argparse
import json
import os
from pathlib import Path
import stat
import subprocess
import tempfile


def disk_info(path, shared=False):
    command = ['qemu-img', 'info', '-f', 'qcow2', '--output=json']
    if shared:
        command.append('--force-share')
    return json.loads(subprocess.check_output([*command, str(path)], text=True, timeout=15))


def disk_identity(path):
    info = path.stat()
    return info.st_dev, info.st_ino


def raise_scan_error(error):
    if not isinstance(error, FileNotFoundError):
        raise error


def qcow_disks(roots):
    seen = set()
    user_tmp = Path(tempfile.gettempdir()).resolve()
    system_tmp = Path('/tmp').resolve()
    for root in roots:
        if not root.exists():
            continue
        for directory, subdirs, names in os.walk(root, onerror=raise_scan_error):
            subdirs[:] = [name for name in subdirs if name not in
                          {'.git', 'target', 'node_modules', 'registry'}]
            if user_tmp != system_tmp and Path(directory).is_relative_to(user_tmp):
                # macOS reserves these service directories inside the user's TMPDIR.
                subdirs[:] = [name for name in subdirs
                              if name != 'TemporaryItems' and not name.startswith('com.apple.')]
            for name in names:
                path = Path(directory, name)
                if path.suffix.lower() not in {'.qcow2', '.img', '.raw'}:
                    continue
                try:
                    if not stat.S_ISREG(path.stat().st_mode):
                        continue
                    if path in seen:
                        continue
                    seen.add(path)
                    with path.open('rb') as image:
                        if image.read(4) != b'QFI\xfb':
                            if path.suffix.lower() == '.qcow2':
                                raise ValueError(f'Unrecognized QCOW2 image: {path}')
                            continue
                except FileNotFoundError:
                    continue
                yield path


def remove_disks(run_dir, names, extra_roots=()):
    disks = {}
    for name in names:
        if Path(name).name != name or Path(name).suffix != '.qcow2':
            raise ValueError(f'Expected a QCOW2 filename within {run_dir}: {name}')
        path = run_dir / name
        if path.is_symlink():
            raise ValueError(f'Refusing to remove a symbolic link: {path}')
        if path.exists():
            disks[path] = disk_identity(path)
    if not disks:
        return

    roots = {root.resolve() for root in
             (run_dir, Path(tempfile.gettempdir()), Path('/tmp'),
              Path.home() / 'atrium-vm', Path.home() / 'atrium-vm-amd64',
              *extra_roots)}
    dependencies = {}
    # Include every image, even an image whose own removal was requested.
    # Leaving a backing chain intact is preferable to guessing its lifetime.
    for image in qcow_disks(sorted(roots)):
        info = disk_info(image, shared=True)
        backing = info.get('full-backing-filename')
        if backing:
            target = image.parent / backing
            try:
                dependencies[disk_identity(target)] = image
            except FileNotFoundError:
                continue

    for path, identity in disks.items():
        if identity in dependencies:
            print(f'Kept {path} because {dependencies[identity]} depends on it.')
            continue
        disk_info(path)
        if disk_identity(path) != identity:
            raise ValueError(f'Disk changed during cleanup: {path}')
        path.unlink()
        print(f'Removed disposable disk {path}.')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('run_dir', type=Path)
    parser.add_argument('disks', nargs='+')
    args = parser.parse_args()
    extra_roots = [Path(root).expanduser() for root in
                   os.environ.get('ATRIUM_VM_IMAGE_ROOTS', '').split(os.pathsep) if root]
    try:
        remove_disks(args.run_dir.resolve(), args.disks, extra_roots)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        parser.exit(1, f'Disposable VM cleanup stopped: {error}\n')


if __name__ == '__main__':
    main()

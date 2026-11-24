#!/usr/bin/env python3
"""Link the staged arm64 engine objects and the unified interpreter/painter archive."""
import argparse
import os
import subprocess
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("stage", type=Path)
parser.add_argument("--sysroot", type=Path, default=Path.home() / "src/bsd/sysroot")
parser.add_argument("--cxx", default="/opt/homebrew/opt/llvm/bin/clang++")
parser.add_argument("--ld", default="/opt/homebrew/bin/ld.lld")
parser.add_argument("--fork-check", action="store_true", help="link the offline I5 fork gate")
args = parser.parse_args()
stage = args.stage.resolve()
objects = stage / "font-rebuild/CMakeFiles"
entry = "fork-check" if args.fork_check else "editor-check"
inputs = [objects / f"editor-host.dir/{entry}.cpp.o",
          objects / "editor-host.dir/EditorPage.cpp.o",
          objects / "font-check.dir/PinnedFonts.cpp.o"]
duplicates = {"font-check.o", "Platform-FontPlugin.o", "navigator-ladybird-PinnedFonts.o"}
inputs.extend(sorted(p for p in (stage / "font-link").glob("*.o") if p.name not in duplicates))
inputs.extend(sorted((stage / "cpp-engine").rglob("*.o")))
lib = stage / "engine-deps/lib"
inputs.extend([stage / "psl-build/src/libpsl.a", lib / "libharfbuzz.a",
               lib / "libsimdutf.a", lib / "libzmij.a", lib / "mimalloc-2.2/libmimalloc.a",
               stage / "ladybird-target/aarch64-unknown-freebsd/release/libnavigator_ladybird_painter.a",
               lib / "libicui18n.a", lib / "libicuuc.a", lib / "libicudata.a"])
response = stage / ("fork-link-inputs.rsp" if args.fork_check else "editor-link-inputs.rsp")
response.write_text("".join('"' + os.fspath(p).replace('\\', '\\\\').replace('"', '\\"') + '"\n' for p in inputs))
subprocess.run([args.cxx, "--target=aarch64-unknown-freebsd", f"--sysroot={args.sysroot}",
                f"--ld-path={args.ld}", "-Wl,--gc-sections",
                *(["-Wl,--wrap=navigator_scene_sample"] if args.fork_check else []), "-o", stage / entry,
                f"@{response}", "-lpthread", "-lm", "-lcrypto", "-lssl", "-lz", "-lkvm",
                "-lexecinfo"], check=True)

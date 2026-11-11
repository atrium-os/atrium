# hospes-probe: the H0 measurement harness (Linux x86_64 only)

Test tooling, not product code (LANGUAGE-POLICY's pragmatic test clause). It runs a closed
Linux library under real glibc and records what Hospes will have to provide. Report:
`docs/reviews/2026-11-11-hospes-h0.md`. Spec: `docs/spec/hospes.md` §10.

| File | What |
|---|---|
| `tracer.c` | ptrace tracer: every syscall counted by **calling module** (so raw syscalls from the library's own text show up), plus opened paths, exec mappings, signal-handler queries, prctl ops |
| `audit.c` | `LD_AUDIT` module: a counting trampoline on every function the target library imports, and argument logs for `dlsym`, `getenv`, `syscall`, `sysconf`, `getauxval` |
| `h264host.c` | drives Cisco's OpenH264 decoder over an Annex-B file; optional raw YUV output for bit-exact comparison |
| `cdmhost.cpp` | a minimal Chromium-interface CDM host: Initialize, a licence request from a Widevine PSSH, a no-key Decrypt; in-memory FileIO; no network |
| `third_party/cdm/` | Chromium's `content_decryption_module*.h` (BSD), from Mozilla's copy, with Mozilla's enum macro replaced by a plain enum |
| `third_party/openh264/` | OpenH264 v2.6.0 API headers (BSD-2) |
| `h0-data/` | imports/exports per library, calls actually made, traces (2026-11-11) |

The libraries themselves are never committed. Build and run:

```sh
gcc -O2 -o tracer tracer.c
gcc -O2 -shared -fPIC -o audit.so audit.c
gcc -O2 -Ithird_party -o h264host h264host.c -ldl   # third_party/wels and third_party/cdmapi are symlinks
g++ -O1 -std=c++17 -Ithird_party -o cdmhost cdmhost.cpp -ldl
H0_TARGET=libopenh264 H0_AUDIT_OUT=oh.audit LD_AUDIT=$PWD/audit.so ./tracer oh.trace ./h264host libopenh264-2.6.0-linux64.8.so in.h264 1
H0_TARGET=libwidevinecdm H0_AUDIT_OUT=wv.audit LD_AUDIT=$PWD/audit.so ./tracer wv.trace ./cdmhost libwidevinecdm.so
```


No disassembly is used anywhere: only the dynamic symbol table, and the library's behaviour
under tracing.

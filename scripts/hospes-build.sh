#!/bin/sh
# Hospes H1: build the loader fixtures and the fixture runner, and run the fixtures.
#
#   sh scripts/hospes-build.sh fixtures            # rebuild hospes/fixtures/bin/{x86_64,aarch64}
#   sh scripts/hospes-build.sh runner TRIPLE       # static no_std runner for TRIPLE
#   sh scripts/hospes-build.sh codec TRIPLE        # hospes-codec (OpenH264 host) for TRIPLE
#   sh scripts/hospes-build.sh test-linux          # x86_64 native + aarch64 under qemu-user
#   sh scripts/hospes-build.sh test RUNNER FIXDIR  # run every fixture with RUNNER (any OS)
#   sh scripts/hospes-build.sh conform CODEC LIB STREAMDIR REF.sha  # H2 conformance (any OS)
#   sh scripts/hospes-build.sh cdm TRIPLE          # hospes-cdm (CDM host) for TRIPLE
#   sh scripts/hospes-build.sh test-cdm            # H3: rebuild the test CDM, its vectors and
#                                                  #   the reference transcript (Linux)
#   sh scripts/hospes-build.sh cdm-check-linux     # H3: hospes-cdm vs the reference host,
#                                                  #   x86_64 native + aarch64 under qemu-user
#   sh scripts/hospes-build.sh cdm-check CHECK CDM ARCH  # H3 on any OS: run hospes-cdm-check
#                                                  #   and diff with the committed transcript
#
# conform: decode every stream in STREAMDIR with hospes-codec --file and compare the I420
# output's SHA-256 with REF.sha ("<sha256> <stream name>" lines), made by the H0 reference
# host (hospes/probe/h264host.c) running the same OpenH264 binary under real glibc.
#
# The fixtures are -nostdlib shared objects built with clang + lld for both machines; every
# import comes from the runner's host table, so they test the loader before the glibc shim
# exists (docs/spec/hospes.md §11.1). The built .so files are committed so the FreeBSD VMs
# run exactly what Linux ran.
#
# The runner is #![no_std] with its own _start: built static, non-PIE, with rust-lld and
# -Z build-std=core,alloc (the repo's .cargo/config asks for build-std of std, which a
# no_std binary does not want; the command line overrides it).
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
H="$ROOT/hospes"

build_fixtures() {
    src="$H/fixtures/src"
    for arch in x86_64 aarch64; do
        out="$H/fixtures/bin/$arch"
        mkdir -p "$out"
        cc="clang --target=$arch-linux-gnu -fuse-ld=lld -nostdlib -ffreestanding -fPIC -O1 -g0"
        $cc -shared -Wl,--version-script="$src/host-stub.map" -Wl,-soname,libhost.so \
            -o "$out/libhost.so" "$src/host-stub.c"
        lk="-shared -Wl,-z,now -Wl,-z,relro -Wl,--hash-style=gnu -L$out -lhost -Wl,--build-id=none"
        $cc $lk -o "$out/reloc.so" "$src/reloc.c"
        $cc $lk -Wl,-z,pack-relative-relocs -Wl,--hash-style=sysv -o "$out/reloc-relr.so" "$src/reloc.c"
        $cc $lk -o "$out/tls.so" "$src/tls.c"
        $cc $lk -o "$out/ifunc.so" "$src/ifunc.c"
        $cc $lk -Wl,-init=fixture_dt_init -o "$out/init.so" "$src/init.c"
        $cc $lk -fstack-protector-all -o "$out/canary.so" "$src/canary.c"
        $cc $lk -o "$out/unknown.so" "$src/unknown.c"
        $cc $lk -o "$out/badversion.so" "$src/badversion.c"
        $cc $lk -o "$out/probe.so" "$src/probe.c"
        rm -f "$out/libhost.so"
        echo "fixtures: $out"
    done
}

# The H3 test CDM (C++, test-only) and the Linux reference host. The .so files, vectors and
# the reference transcript are committed, so the FreeBSD VMs check against what Linux ran.
T="$H/test-cdm"
build_test_cdm() {
    f="-O2 -fPIC -shared -fno-exceptions -fno-rtti -fvisibility=hidden -fstack-protector-strong -std=c++17 -Wall -Wextra"
    f="$f -I$H/probe/third_party -nostdlib++ -Wl,--version-script=$T/exports.map -Wl,--build-id=none"
    mkdir -p "$T/bin" "$T/vectors"
    clang++ --target=x86_64-linux-gnu -fuse-ld=lld $f -o "$T/bin/x86_64-clearkey.so" "$T/clearkey.cc" "$T/aes.cc"
    clang++ --target=aarch64-linux-gnu -fuse-ld=lld $f -o "$T/bin/aarch64-clearkey.so" "$T/clearkey.cc" "$T/aes.cc"
    python3 "$T/make-vectors.py" "$T/vectors"
    tmp=$(mktemp -d)
    g++ -O1 -std=c++17 -Wall -I"$H/probe/third_party" -o "$tmp/refhost" "$T/refhost.cc" -ldl
    aarch64-linux-gnu-g++ -O1 -std=c++17 -I"$H/probe/third_party" -o "$tmp/refhost-arm64" "$T/refhost.cc" -ldl
    "$tmp/refhost" "$T/bin/x86_64-clearkey.so" "$T/vectors" > "$T/vectors/transcript.txt"
    qemu-aarch64-static -L /usr/aarch64-linux-gnu "$tmp/refhost-arm64" "$T/bin/aarch64-clearkey.so" "$T/vectors" > "$tmp/arm.txt"
    diff "$T/vectors/transcript.txt" "$tmp/arm.txt" >&2 || { echo "reference transcripts differ between machines" >&2; exit 1; }
    rm -rf "$tmp"
    echo "test CDM, vectors and transcript: $T" >&2
}

# cdm_check CHECK CDM ARCH [WRAPPER]: run the scenario, diff with the reference transcript.
cdm_check() {
    check="$1"; cdm="$2"; arch="$3"; wrap="${4:-}"
    out="${TMPDIR:-/tmp}/hospes-cdm-check.$$"
    mkdir -p "$out"
    if [ -n "$wrap" ]; then
        printf '#!/bin/sh\nexec %s %s "$@"\n' "$wrap" "$cdm" > "$out/cdm.sh"; chmod +x "$out/cdm.sh"; cdm="$out/cdm.sh"
    fi
    if "$check" "$cdm" "$T/bin/$arch-clearkey.so" "$T/vectors" > "$out/transcript.txt" 2> "$out/err.txt" &&
        diff "$T/vectors/transcript.txt" "$out/transcript.txt" >&2; then
        echo "hospes-cdm ($arch): transcript equals the reference host's ($(wc -l < "$out/transcript.txt") lines)" >&2
        rm -rf "$out"
    else
        echo "hospes-cdm ($arch): MISMATCH; see $out" >&2; cat "$out/err.txt" >&2; return 1
    fi
}

# build_bin TRIPLE PACKAGE: a static no_std Hospes binary.
build_bin() {
    triple="$1"; pkg="$2"
    cd "$H"
    RUSTFLAGS="-C relocation-model=static -C target-feature=+crt-static -C linker=rust-lld -C linker-flavor=ld.lld -C link-arg=-static -C link-arg=--no-dynamic-linker -C link-arg=-z -C link-arg=noexecstack" \
        cargo build --release -p "$pkg" --target "$triple" \
        -Z build-std=core,alloc -Z build-std-features=compiler-builtins-mem \
        --config 'profile.release.panic="abort"' >&2
    echo "$H/target/$triple/release/$pkg"
}

build_runner() { build_bin "$1" hospes-fixture-runner; }

# run_all RUNNER FIXDIR [WRAPPER]
run_all() {
    r="$1"; d="$2"; w="${3:-}"
    fails=0
    for f in reloc reloc-relr tls ifunc init canary; do
        $w "$r" run "$d/$f.so" || fails=$((fails + 1))
    done
    for f in unknown badversion; do
        $w "$r" refuse "$d/$f.so" || fails=$((fails + 1))
    done
    $w "$r" probe "$d/probe.so" || fails=$((fails + 1))
    for m in relro-write text-write; do
        set +e
        $w "$r" "$m" "$d/reloc.so" 2>/dev/null
        rc=$?
        set -e
        # SIGSEGV (139) or SIGBUS (138) through the shell.
        if [ "$rc" -eq 139 ] || [ "$rc" -eq 138 ]; then
            echo "PASS $m (faulted, status $rc)" >&2
        else
            echo "FAIL $m: status $rc, expected a fault" >&2
            fails=$((fails + 1))
        fi
    done
    echo "hospes fixtures: $fails failed" >&2
    [ "$fails" -eq 0 ]
}

sha() { if command -v sha256sum >/dev/null; then sha256sum < "$1" | cut -d' ' -f1; else sha256 -q "$1"; fi; }

conform() {
    codec="$1"; lib="$2"; dir="$3"; ref="$4"
    out="${TMPDIR:-/tmp}/hospes-conform.$$"
    mkdir -p "$out"
    fails=0; n=0
    while read -r want name; do
        n=$((n + 1))
        if ! $codec --file "$lib" "$dir/$name" "$out/$name.yuv" 2>"$out/$name.log"; then
            echo "FAIL $name: $(tail -1 "$out/$name.log")" >&2; fails=$((fails + 1)); continue
        fi
        got=$(sha "$out/$name.yuv")
        if [ "$got" = "$want" ]; then
            echo "MATCH $name ($(tail -1 "$out/$name.log" | sed 's/hospes-codec: //'))" >&2
        else
            echo "DIFF $name" >&2; fails=$((fails + 1))
        fi
    done < "$ref"
    rm -rf "$out"
    echo "hospes conformance: $((n - fails))/$n match" >&2
    [ "$fails" -eq 0 ]
}

case "${1:-}" in
    fixtures) build_fixtures ;;
    runner) build_runner "$2" ;;
    codec) build_bin "$2" hospes-codec ;;
    cdm) build_bin "$2" hospes-cdm ;;
    test-cdm) build_test_cdm ;;
    cdm-check) cdm_check "$2" "$3" "$4" ;;
    cdm-check-linux)
        x=$(build_bin x86_64-unknown-linux-gnu hospes-cdm)
        a=$(build_bin aarch64-unknown-linux-gnu hospes-cdm)
        (cd "$H" && cargo build --release -p hospes-cdm-check >&2)
        rc=0
        cdm_check "$H/target/release/hospes-cdm-check" "$x" x86_64 || rc=1
        cdm_check "$H/target/release/hospes-cdm-check" "$a" aarch64 qemu-aarch64-static || rc=1
        exit $rc ;;
    test) run_all "$2" "$3" ;;
    conform) conform "$2" "$3" "$4" "$5" ;;
    test-linux)
        x=$(build_runner x86_64-unknown-linux-gnu)
        a=$(build_runner aarch64-unknown-linux-gnu)
        echo "== x86_64 (native)" >&2
        rc=0
        run_all "$x" "$H/fixtures/bin/x86_64" || rc=1
        echo "== aarch64 (qemu-aarch64-static)" >&2
        run_all "$a" "$H/fixtures/bin/aarch64" qemu-aarch64-static || rc=1
        exit $rc ;;
    *) sed -n '2,8p' "$0"; exit 2 ;;
esac

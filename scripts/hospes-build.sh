#!/bin/sh
# Hospes H1: build the loader fixtures and the fixture runner, and run the fixtures.
#
#   sh scripts/hospes-build.sh fixtures            # rebuild hospes/fixtures/bin/{x86_64,aarch64}
#   sh scripts/hospes-build.sh runner TRIPLE       # static no_std runner for TRIPLE
#   sh scripts/hospes-build.sh test-linux          # x86_64 native + aarch64 under qemu-user
#   sh scripts/hospes-build.sh test RUNNER FIXDIR  # run every fixture with RUNNER (any OS)
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

build_runner() {
    triple="$1"
    cd "$H"
    RUSTFLAGS="-C relocation-model=static -C target-feature=+crt-static -C linker=rust-lld -C linker-flavor=ld.lld -C link-arg=-static -C link-arg=--no-dynamic-linker -C link-arg=-z -C link-arg=noexecstack" \
        cargo build --release -p hospes-fixture-runner --target "$triple" \
        -Z build-std=core,alloc -Z build-std-features=compiler-builtins-mem \
        --config 'profile.release.panic="abort"' >&2
    echo "$H/target/$triple/release/hospes-fixture-runner"
}

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

case "${1:-}" in
    fixtures) build_fixtures ;;
    runner) build_runner "$2" ;;
    test) run_all "$2" "$3" ;;
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

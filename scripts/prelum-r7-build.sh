#!/bin/sh
# Host-only cross-build for the desktop IPPS gate.
set -eu
repo=$1
out=$2
target=aarch64-unknown-freebsd

# Native code in ring and the mail parser also needs the FreeBSD headers.
: "${CC_aarch64_unknown_freebsd:=clang}"
: "${CFLAGS_aarch64_unknown_freebsd:=--sysroot=/Users/girivs/src/bsd/sysroot}"
: "${AR_aarch64_unknown_freebsd:=$(command -v llvm-ar || command -v ar)}"
export CC_aarch64_unknown_freebsd CFLAGS_aarch64_unknown_freebsd AR_aarch64_unknown_freebsd

mkdir "$out/bin" "$out/build"
build() {
    crate=$1
    selector=$2
    name=$3
    log="$out/build/$name.log"
    printf 'Cross-building %s\n' "$name"
    if ! (cd "$repo/$crate" && cargo build --locked --release --target "$target" \
        --target-dir "$out/target" "$selector" "$name") >"$log" 2>&1; then
        tail -60 "$log" >&2
        return 1
    fi
    subdir=
    if [ "$selector" = --example ]; then subdir=examples/; fi
    cp "$out/target/$target/release/$subdir$name" "$out/bin/$name"
    digest=$(openssl dgst -sha256 <"$out/bin/$name" | awk '{print $NF}')
    printf '%s  %s\n' "$digest" "$name" >>"$out/built.sha256"
}

for app in scrinium atrium-sheets atrium-slides; do
    build "$app" --bin "$app"
done
build mail/atrium-mail --bin atrium-mail
build mail/atrium-mail --example prelum-print-fixture
build mail/mail-parse --bin mail-parse-jail
build officina-convert --bin officina-convert
build navigator-page --bin navigator-page
build atrium-pdf --bin atrium-pdf
build frescod --bin frescod
build limen --bin limend
build prelum-sheet --bin atrium-prelum-sheet
for program in atrium-prelumd prelumctl atrium-prelum-ipp atrium-prelum-render; do
    build prelum --bin "$program"
done

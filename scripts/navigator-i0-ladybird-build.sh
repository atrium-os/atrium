#!/bin/sh
# Cross-build the editor's Rust engine libraries and generated Web IDL bindings.
# Use a scratch checkout of Ladybird ead47ae6ef846b910ac509f51b12281523d7c2c3.
# Install Rust 1.98.0 with rust-src on the Mac. Run this script with Homebrew cmake on PATH.
# Apply patches/ladybird/freebsd-arm64-wasm-interpreter.patch in that checkout.
# LLVM_LIBC_INCLUDE_DIR points to the pinned LLVM libc/ directory containing shared/math.h
# from llvm-project e54169423e7ee5d8b41c2f1b6f7a59000c901ee0.
set -eu
[ "$#" = 2 ] || { echo "usage: $0 LADYBIRD_SOURCE BUILD_DIRECTORY" >&2; exit 2; }
src=$(cd "$1" && pwd)
mkdir -p "$2"
out=$(cd "$2" && pwd)
sysroot=${ATRIUM_SYSROOT:-$HOME/src/bsd/sysroot}
llvm=${LLVM_BIN:-/opt/homebrew/opt/llvm/bin}
test -f "${LLVM_LIBC_INCLUDE_DIR:?set the path to the LLVM libc headers}/shared/math.h"
test -f "$sysroot/usr/lib/Scrt1.o"
test -f "$src/Libraries/LibWasm/Engine/src/runtime/interpreter_only.rs"

cd "$src"
target=aarch64-unknown-freebsd
tree="$out/bindings-dependencies.txt"
cargo +1.98.0 tree --locked --target "$target" -p libweb_bindings_rust \
    --no-default-features --edges normal --prefix none > "$tree"
if grep -Eq '^(libwasm_jit|libjs_jit) ' "$tree"; then
    echo "A JIT compiler is reachable from the target bindings" >&2
    exit 1
fi

web="$src/Libraries/LibWeb"
gen="$out/generated"
mkdir -p "$gen/CSS" "$gen/web" "$gen/common"
python3 Meta/Generators/generate_libweb_css_numeric_factory_methods.py \
    -i "$gen/CSS/GeneratedCSSNumericFactoryMethods.idl" -j "$web/CSS/Units.json"
# Evaluate the same IDL inventory included by upstream libweb_generators.cmake.
cat > "$gen/inputs.cmake" <<'CMAKE'
function(libweb_js_bindings name)
    if(name STREQUAL "CSS/GeneratedCSSNumericFactoryMethods")
        set(path "${gen}/${name}.idl")
    else()
        set(path "${web}/${name}.idl")
    endif()
    file(APPEND "${gen}/idl.txt" "${path}\n")
endfunction()
file(WRITE "${gen}/idl.txt" "")
include("${web}/idl_files.cmake")
CMAKE
cmake "-Dweb=$web" "-Dgen=$gen" -P "$gen/inputs.cmake"
python3 Meta/Generators/generate_libweb_bindings.py -o "$gen/web" \
    --common-output-path "$gen/common" "@$gen/idl.txt"

export LIBWEB_RUST_BINDINGS="$gen/web/RustBindings.rs"
export CARGO_TARGET_DIR="$out/target" RUSTC_BOOTSTRAP=1
export CC_aarch64_unknown_freebsd="$llvm/clang" CXX_aarch64_unknown_freebsd="$llvm/clang++"
export AR_aarch64_unknown_freebsd="$llvm/llvm-ar" CXXSTDLIB_aarch64_unknown_freebsd=c++
export CFLAGS_aarch64_unknown_freebsd="--target=$target --sysroot=$sysroot"
export CXXFLAGS_aarch64_unknown_freebsd="$CFLAGS_aarch64_unknown_freebsd"
cargo +1.98.0 build --locked -Z build-std=std,panic_abort --release --target "$target" \
    --no-default-features --lib -j "${I0_BUILD_JOBS:-4}" \
    -p libjs_rust -p libcompositing_rust -p libweb_rust -p libweb_bindings_rust

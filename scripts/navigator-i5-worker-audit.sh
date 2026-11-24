#!/bin/sh
# Compare the installed engine digest with a stripped copy of the symbol-bearing link.
set -eu
if [ "$#" -ne 2 ]; then
    echo "usage: $0 UNSTRIPPED_ENGINE INSTALLED_SHA256" >&2
    exit 2
fi
worker=$1
expected=$2
llvm=${LLVM_BIN:-/opt/homebrew/opt/llvm/bin}
stage=$(mktemp -d)
trap 'rm -f "$stage/worker" "$stage/symbols"; rmdir "$stage"' EXIT
"$llvm/llvm-strip" -o "$stage/worker" "$worker"
actual=$(shasum -a 256 "$stage/worker" | awk '{print $1}')
[ "$actual" = "$expected" ] || { echo 'installed worker digest differs' >&2; exit 1; }
"$llvm/llvm-nm" --defined-only --demangle "$worker" >"$stage/symbols"
for entry in js_interpreter wasm_interpreter; do
    grep -E " [Tt] $entry$" "$stage/symbols"
done
if grep -Eq 'lib(js|wasm)_jit::' "$stage/symbols"; then
    echo 'found linked Ladybird JIT compiler symbols' >&2
    exit 1
fi
echo "Verified installed SHA256 $actual, both interpreter entry points and no LibJS/LibWasm JIT crate symbols."

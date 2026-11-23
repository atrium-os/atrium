#!/bin/sh
set -eu
cd "$(dirname "$0")"
test "$(uname -s)/$(uname -m)" = FreeBSD/arm64
uname -a
mount -p | awk '$2 == "/" && $3 == "tessera" { print; ok = 1 } END { exit !ok }'
openssl version
while read -r digest binary; do
    test "$(sha256 -q "$binary")" = "$digest"
done < sums

run() {
    bin=$1
    expected=$2
    shift 2
    timeout 30 "./$bin" --list "$@" >"$bin.inventory"
    test "$(grep -c ': test$' "$bin.inventory")" = "$expected"
    awk '/: test$/ { sub(/: test$/, ""); print }' "$bin.inventory" | sort >"$bin.expected"
    if ! timeout 90 "./$bin" --test-threads=1 "$@" >"$bin.results" 2>&1; then
        cat "$bin.results"
        return 1
    fi
    cat "$bin.results"
    awk '/^test .* \.\.\. ok$/ { sub(/^test /, ""); sub(/ \.\.\. ok$/, ""); print }' "$bin.results" | sort >"$bin.passed"
    diff -u "$bin.expected" "$bin.passed"
}
run arcad-unit 6 server::smime
run mail-smime-unit 3
run mail-smime-interop 5
grep -Fxq 'test server::smime::legacy::tests::legacy_requires_explicit_approval_and_uses_the_native_provider ... ok' arcad-unit.results
grep -Fxq 'test envelope::legacy_tests::legacy_routes_only_an_unambiguous_legacy_recipient ... ok' mail-smime-unit.results
echo MAIL_LEGACY_SMIME_PROTOTYPE_TESSERA_OK

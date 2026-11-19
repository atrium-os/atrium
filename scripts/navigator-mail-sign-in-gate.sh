#!/bin/sh
# Guest-side development gate. Stage the arm64 broker integration test and
# the signed I0_SIGN_IN Ladybird bundle on guest-local storage first.
set -eu
[ "$#" = 2 ] || { echo "usage: $0 BROKER_TEST SIGNED_ENGINE_BUNDLE" >&2; exit 2; }
test "$(uname -m)" = arm64
test "$(mount -p | awk '$2 == "/" {print $3}')" = tessera
umask 077
dir=$(mktemp -d /tmp/navigator-mail.XXXXXX)
echo "Development gate evidence in $dir"
I0_DEVELOPMENT_DOCUMENT="$dir/page.html" "$1" --exact microsoft_sheet_claims_mail_session_and_fetches_with_its_own_cookies >"$dir/broker.log" 2>&1 || { cat "$dir/broker.log"; exit 1; }
grep -F '1 passed; 0 failed' "$dir/broker.log"
test -s "$dir/page.html"
# Portcullis creates traversable jail directories. The evidence directory stays private.
umask 022
timeout 45 /usr/local/bin/portcullis exec --require-memory-limit --user alice --instance "mailtest$$" "$2" <"$dir/page.html" >"$dir/engine.log" 2>&1 || { cat "$dir/engine.log"; exit 1; }
cat "$dir/engine.log"
grep -Eq '^jailed=1 uid=[1-9][0-9]* capmode=1 denied-path=ECAPMODE$' "$dir/engine.log"
grep -Fx 'development_account_input=1 inline_script=1 native_scene=1' "$dir/engine.log"
grep -Eq '^editor scene node changes=[1-9][0-9]*$' "$dir/engine.log"

#!/bin/sh
# Install the one-shot I4 entry fetcher inside the test guest.
# The optional hosts file maps the public provider to the gate's TLS forward.
set -eu
bin=$1 key=$2 ca=$3
app=org.atrium.navigator.youtube-entry
bundle=$(mktemp -d /tmp/navigator-entry.XXXXXX)
mkdir -p "$bundle/bin" "$bundle/etc/ssl"
cp "$bin" "$bundle/bin/navigator-youtube-entry"
cp "$ca" "$bundle/etc/ssl/cert.pem"
cp /etc/resolv.conf /etc/nsswitch.conf "$bundle/etc/"
cp "${4:-/etc/hosts}" "$bundle/etc/hosts"
cat > "$bundle/atrium.toml" <<T
[app]
id="$app"
name="Navigator YouTube entry"
version="1"
entry="bin/navigator-youtube-entry"
[capabilities]
network="full"
[resources]
memory="128M"
cpu=100
T
openssl dgst -sha256 -sign "$key" -out "$bundle/atrium.toml.sig" "$bundle/atrium.toml"
opifex install "$bundle"
portcullis policy grant --system "$app"
policy=/etc/atrium/policy.toml
if ! grep -Fxq "[workers.\"$app\"]" "$policy"; then
    cat >> "$policy" <<T

[workers."$app"]
caller_apps = ["org.atrium.navigator"]
run_as = "_navpage"
per_instance = true
concurrent = 3
T
fi

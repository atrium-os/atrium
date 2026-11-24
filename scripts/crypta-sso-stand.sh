#!/bin/sh
# The SAML stand for Crypta's AnyConnect single sign-on (vpn.md §9.3, §11),
# on the host beside crypta-ocserv-vm.sh's forwards:
#
#   sh scripts/crypta-sso-stand.sh up      Keycloak and the stand-in gateway
#   sh scripts/crypta-sso-stand.sh token   a browser's sign-in from the host: the gateway's token
#   sh scripts/crypta-sso-stand.sh webauthn   security keys required (on|off|reset)
#   sh scripts/crypta-sso-stand.sh down
#
# - Keycloak (Apache-2.0; KEYCLOAK_HOME, default ~/keycloak/keycloak-26.0.7)
#   is the identity provider: realm `corp`, user alice (password
#   alice-sso-pw), and a SAML client for the gateway (signed assertions,
#   HTTP-POST to its assertion consumer service). HTTPS on 127.0.0.1:8448 as
#   https://idp.crypta.test:448 (lan.py carries the guest's
#   198.51.100.52:448 there), its certificate from the gate's CA.
# - scripts/crypta-sso-standin.py is the gateway, on 127.0.0.1:8447 as
#   https://sso.crypta.test:447 (lan.py: 198.51.100.52:447): SAML in front,
#   ocserv's password gateway (:8443) behind for the tunnel.
# Test tools only; nothing here ships.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
D=${CRYPTA_LINUX_DIR:-$HOME/crypta-linux-vm}
P=$D/pki
S=$D/sso
KC=${KEYCLOAK_HOME:-$HOME/keycloak/keycloak-26.0.7}
GW=https://sso.crypta.test:447
IDP=https://idp.crypta.test:448
REALM_SSO=$IDP/realms/corp/protocol/saml
ADM="$KC/bin/kcadm.sh"

cert() { # $1 name: a server certificate from the gate CA
    [ -f "$S/$1.pem" ] && return 0
    openssl req -new -newkey rsa:2048 -nodes -keyout "$S/$1.key" -subj "/CN=$1" -out "$S/$1.csr" 2>/dev/null
    printf 'subjectAltName=DNS:%s\nextendedKeyUsage=serverAuth\n' "$1" > "$S/$1.ext"
    openssl x509 -req -in "$S/$1.csr" -CA "$P/ca.pem" -CAkey "$P/ca.key" -CAcreateserial -days 365 -extfile "$S/$1.ext" -out "$S/$1.pem" 2>/dev/null
}

case "${1:-}" in
up)
    mkdir -p "$S"
    cert sso.crypta.test
    cert idp.crypta.test
    [ -x "$KC/bin/kc.sh" ] || { echo "no Keycloak at $KC" >&2; exit 1; }
    if ! curl -s -o /dev/null --max-time 2 http://127.0.0.1:8180/realms/master; then
        pkill -f 'keycloak.*quarkus' 2>/dev/null || true
        ( cd "$KC" && KC_BOOTSTRAP_ADMIN_USERNAME=admin KC_BOOTSTRAP_ADMIN_PASSWORD=admin-gate \
            nohup bin/kc.sh start-dev --http-enabled=true --http-host=127.0.0.1 --http-port=8180 \
            --https-port=8448 --https-certificate-file="$S/idp.crypta.test.pem" --https-certificate-key-file="$S/idp.crypta.test.key" \
            --hostname="$IDP" --hostname-backchannel-dynamic=true --hostname-strict=false > "$S/keycloak.log" 2>&1 & )
        i=0; until curl -s -o /dev/null --max-time 2 http://127.0.0.1:8180/realms/master; do
            i=$((i + 1)); [ $i -lt 180 ] || { tail -20 "$S/keycloak.log" >&2; exit 1; }; sleep 2; done
    fi
    "$ADM" config credentials --server http://127.0.0.1:8180 --realm master --user admin --password admin-gate >/dev/null
    "$ADM" get realms/corp >/dev/null 2>&1 || "$ADM" create realms -s realm=corp -s enabled=true >/dev/null
    if [ -z "$("$ADM" get users -r corp -q username=alice --fields id 2>/dev/null | grep id || true)" ]; then
        "$ADM" create users -r corp -s username=alice -s enabled=true -s email=alice@corp.example -s emailVerified=true \
            -s firstName=Alice -s lastName=Gate >/dev/null
        "$ADM" set-password -r corp --username alice --new-password alice-sso-pw
    fi
    ENTITY="$GW/+CSCOE+/saml/sp/metadata"
    if [ -z "$("$ADM" get clients -r corp -q clientId="$ENTITY" --fields id | grep id || true)" ]; then
        "$ADM" create clients -r corp -s clientId="$ENTITY" -s protocol=saml -s enabled=true \
            -s "redirectUris=[\"$GW/*\"]" \
            -s 'attributes."saml_assertion_consumer_url_post"='"$GW/+CSCOE+/saml/sp/acs" \
            -s 'attributes."saml.client.signature"=false' -s 'attributes."saml.assertion.signature"=true' \
            -s 'attributes."saml.server.signature"=true' -s 'attributes."saml_name_id_format"=username' \
            -s 'attributes."saml.force.name.id.format"=true' -s frontchannelLogout=false >/dev/null
    fi
    # The IdP's signing certificate, for the stand-in's xmlsec1 check.
    "$ADM" get keys -r corp | python3 -c '
import json,sys
for k in json.load(sys.stdin)["keys"]:
    if k.get("algorithm")=="RS256" and k.get("certificate"):
        c=k["certificate"]; print("-----BEGIN CERTIFICATE-----"); [print(c[i:i+64]) for i in range(0,len(c),64)]; print("-----END CERTIFICATE-----"); break' > "$S/idp-signing.pem"
    pkill -f crypta-sso-standin.py 2>/dev/null || true
    IDPC=""; command -v xmlsec1 >/dev/null && IDPC="$S/idp-signing.pem"
    nohup python3 "$ROOT/scripts/crypta-sso-standin.py" 8447 "$S/sso.crypta.test.pem" "$S/sso.crypta.test.key" "$GW" "$REALM_SSO" 8443 "$P/ca.pem" $IDPC > "$S/standin.log" 2>&1 &
    sleep 1
    nc -z 127.0.0.1 8447 || { cat "$S/standin.log" >&2; exit 1; }
    echo "Keycloak at $IDP (127.0.0.1:8448), the stand-in gateway at $GW (127.0.0.1:8447)${IDPC:+, signatures checked}"
    ;;
token)
    python3 "$ROOT/scripts/crypta-sso-browser.py" "$P/ca.pem" "$GW/+CSCOE+/saml/sp/login?tgname=sso" "${2:-alice}" "${3:-alice-sso-pw}" \
        sso.crypta.test:447=127.0.0.1:8447 idp.crypta.test:448=127.0.0.1:8448
    ;;
webauthn)
    # Security keys required (vpn.md "Scripts in the sign-in window"): the
    # realm's browser flow becomes a copy with Keycloak's WebAuthn
    # Authenticator required after the password. A user with no key
    # registers one on the next sign-in (navigator.credentials.create),
    # later sign-ins use it (.get). Stock Keycloak pages and scripts.
    # `webauthn off` puts the plain flow back; `webauthn reset` forgets
    # alice's keys.
    B=http://127.0.0.1:8180/admin/realms/corp
    T=$(curl -s -d client_id=admin-cli -d username=admin -d password=admin-gate -d grant_type=password \
        http://127.0.0.1:8180/realms/master/protocol/openid-connect/token | python3 -c 'import json,sys; print(json.load(sys.stdin)["access_token"])')
    H="Authorization: Bearer $T"
    case "${2:-on}" in
    on)
        if ! curl -sf -o /dev/null -H "$H" "$B/authentication/flows/browser-webauthn/executions"; then
            curl -sf -H "$H" -H 'Content-Type: application/json' -d '{"newName":"browser-webauthn"}' "$B/authentication/flows/browser/copy"
            curl -sf -H "$H" -H 'Content-Type: application/json' -d '{"provider":"webauthn-authenticator"}' \
                "$B/authentication/flows/browser-webauthn%20forms/executions/execution"
            ex=$(curl -sf -H "$H" "$B/authentication/flows/browser-webauthn/executions" | python3 -c 'import json,sys
e=[e for e in json.load(sys.stdin) if e.get("providerId")=="webauthn-authenticator"][0]; e["requirement"]="REQUIRED"; print(json.dumps(e))')
            curl -sf -X PUT -H "$H" -H 'Content-Type: application/json' -d "$ex" "$B/authentication/flows/browser-webauthn/executions"
        fi
        curl -sf -X PUT -H "$H" -H 'Content-Type: application/json' -d '{"browserFlow":"browser-webauthn"}' "$B"
        echo "corp: WebAuthn required after the password"
        ;;
    off)
        curl -sf -X PUT -H "$H" -H 'Content-Type: application/json' -d '{"browserFlow":"browser"}' "$B"
        echo "corp: password only"
        ;;
    reset)
        u=$(curl -sf -H "$H" "$B/users?username=alice&exact=true" | python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["id"])')
        for c in $(curl -sf -H "$H" "$B/users/$u/credentials" | python3 -c 'import json,sys; print(" ".join(c["id"] for c in json.load(sys.stdin) if c["type"]=="webauthn"))'); do
            curl -sf -X DELETE -H "$H" "$B/users/$u/credentials/$c"
        done
        echo "alice: no security keys"
        ;;
    esac
    ;;
down)
    pkill -f crypta-sso-standin.py 2>/dev/null || true
    pkill -f 'keycloak.*quarkus' 2>/dev/null || true
    ;;
*)
    echo "usage: $0 up|token|webauthn [on|off|reset]|down" >&2
    exit 2
    ;;
esac

#!/bin/sh
# A WebDAV server over TLS for the share gates (network-shares.md S2): Apache
# with mod_dav on the Linux host, two ports, Basic auth inside TLS.
#
#   sh scripts/webdav-test-server.sh up DIR [NAME...]   # start
#   sh scripts/webdav-test-server.sh down DIR            # stop
#
# DIR (somewhere www-data can reach, e.g. /srv/davtest) holds everything: a test CA (DIR/ca.pem), the served tree (DIR/dav,
# shown at /dav/), the password file (user davtest, password in
# DIR/password) and the logs. Ports:
#   8443  a certificate from the test CA, for each NAME (DNS) and 10.0.2.2,
#         127.0.0.1 (IP): the "trusted" path once ca.pem is in the trust store
#   8444  a self-signed certificate: the "pinned" path
# Needs apache2 and openssl (apt-get install apache2 openssl).
set -eu
cmd=${1:?up or down}
D=${2:?state directory}
shift 2
mkdir -p "$D"
D=$(cd "$D" && pwd)
MODS=/usr/lib/apache2/modules

down() {
    [ -f "$D/httpd.pid" ] && kill "$(cat "$D/httpd.pid")" 2>/dev/null || true
    i=0; while [ -f "$D/httpd.pid" ] && [ $i -lt 20 ]; do sleep 0.2; i=$((i+1)); done
    rm -f "$D/httpd.pid"
}

if [ "$cmd" = down ]; then down; exit 0; fi
down

if [ ! -f "$D/ca.pem" ]; then
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 30 \
        -subj "/CN=Atrium share test CA" -keyout "$D/ca.key" -out "$D/ca.pem" 2>/dev/null
    san="IP:10.0.2.2,IP:127.0.0.1"
    for n in "$@"; do san="$san,DNS:$n"; done
    openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj "/CN=davtest" \
        -keyout "$D/server.key" -out "$D/server.csr" 2>/dev/null
    printf 'subjectAltName=%s\n' "$san" > "$D/ext"
    openssl x509 -req -in "$D/server.csr" -CA "$D/ca.pem" -CAkey "$D/ca.key" -CAcreateserial \
        -days 30 -extfile "$D/ext" -out "$D/server.pem" 2>/dev/null
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 30 \
        -subj "/CN=my-nas" -addext "subjectAltName=IP:10.0.2.2" \
        -keyout "$D/self.key" -out "$D/self.pem" 2>/dev/null
    head -c 12 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$D/password"
    htpasswd -cb "$D/htpasswd" davtest "$(cat "$D/password")" 2>/dev/null
fi
mkdir -p "$D/dav" "$D/lock" "$D/logs"
chown -R www-data:www-data "$D/dav" "$D/lock"

cat > "$D/httpd.conf" <<EOF
ServerRoot "$D"
PidFile "$D/httpd.pid"
ErrorLog "$D/logs/error.log"
LogLevel warn
User www-data
Group www-data
ServerName davtest
Listen 8443
Listen 8444
LoadModule mpm_event_module $MODS/mod_mpm_event.so
LoadModule authz_core_module $MODS/mod_authz_core.so
LoadModule authz_user_module $MODS/mod_authz_user.so
LoadModule authn_core_module $MODS/mod_authn_core.so
LoadModule authn_file_module $MODS/mod_authn_file.so
LoadModule auth_basic_module $MODS/mod_auth_basic.so
LoadModule alias_module $MODS/mod_alias.so
LoadModule dav_module $MODS/mod_dav.so
LoadModule dav_fs_module $MODS/mod_dav_fs.so
LoadModule ssl_module $MODS/mod_ssl.so
LoadModule socache_shmcb_module $MODS/mod_socache_shmcb.so
LogFormat "%h %u \"%r\" %>s %b" short
CustomLog "$D/logs/access.log" short
DavLockDB "$D/lock/DavLock"
KeepAliveTimeout 3
Alias /dav "$D/dav"
<Directory "$D/dav">
    AllowOverride None
    Dav On
    AuthType Basic
    AuthName "davtest"
    AuthUserFile "$D/htpasswd"
    Require valid-user
</Directory>
SSLSessionCache "shmcb:$D/ssl_scache(512000)"
<VirtualHost *:8443>
    SSLEngine on
    SSLCertificateFile "$D/server.pem"
    SSLCertificateKeyFile "$D/server.key"
</VirtualHost>
<VirtualHost *:8444>
    SSLEngine on
    SSLCertificateFile "$D/self.pem"
    SSLCertificateKeyFile "$D/self.key"
</VirtualHost>
EOF
/usr/sbin/apache2 -f "$D/httpd.conf" -k start
i=0; while [ ! -f "$D/httpd.pid" ] && [ $i -lt 50 ]; do sleep 0.1; i=$((i+1)); done
[ -f "$D/httpd.pid" ] || { cat "$D/logs/error.log" >&2; exit 1; }
echo "webdav: up on 8443 (test CA) and 8444 (self-signed), user davtest"

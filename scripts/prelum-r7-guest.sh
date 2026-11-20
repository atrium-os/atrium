#!/bin/sh
# Run only on the disposable guest started by prelum-r7-vm.sh.
set -eu
P=$1
port=$2
marker=$3
cd "$P"
test "$(cat .isolated-gate)" = "$marker"
test "$(uname -s)" = FreeBSD
test "$(uname -m)" = arm64
for entry in prelumail:58641 prelusheets:58642; do
    user=${entry%:*}
    uid=${entry#*:}
    if ! pw usershow "$user" >/dev/null 2>&1; then
        pw useradd "$user" -u "$uid" -d /nonexistent -s /usr/sbin/nologin
    fi
    test "$(id -u "$user")" = "$uid"
done
policy=/etc/atrium/jaild.policy.toml
if ! grep -q '^\[resource_control\]' "$policy"; then
    printf '\n[resource_control]\nallow_rctl = true\n' >>"$policy"
    service atrium-jaild restart
else
    grep -Eq '^allow_rctl *= *true' "$policy"
fi

mkdir home limen
chown prelusheets home
chown _limen limen
printf '%s\n' '58642 root org.test.prelum.sheetsprint' '58641 root org.test.prelum.mailprint' >registry
daemon -f -p "$P/prelum.pid" -o "$P/prelum.log" "$P/atrium-prelumd" --socket "$P/print.sock" --state "$P/state" --spool "$P/spool" --sheet-uid 65046 --registry "$P/registry"
! sockstat -u -l | grep -q '/atrium/sockets/fresco/fresco.sock'
rm -f /atrium/sockets/fresco/fresco.sock
ln -s /atrium/sockets/fresco/fresco.sock "$P/fresco.sock"


env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG="$P/screen" FRESCOD_SOCK=/atrium/sockets/fresco/fresco.sock FRESCOD_WM_SOCK="$P/wm.sock" FRESCOD_INPUT_SOCK="$P/inject.sock" FRESCOD_ACTIVATION_SOCK="$P/act.sock" FRESCOD_SLOT_UIDS=0,65022 FRESCOD_DEV_CAPS=all FRESCOD_INJECT_ACTIVATION=1 FRESCOD_TOKEN_LIFE_MS=15000 daemon -f -p "$P/fresco.pid" -o "$P/fresco.log" "$P/frescod"
for sock in print fresco act inject; do
    for i in $(seq 30); do [ -S "$P/$sock.sock" ] && break; sleep 1; done
    test -S "$P/$sock.sock"
    chmod 666 "$P/$sock.sock"
done
env LIMEN_DEFAULTS=doc-viewer=org.test.prelum.quickhtml LIMEN_SOCKET="$P/limen/limend.sock" LIMEN_FRESCOD_ACTIVATION="$P/act.sock" LIMEN_APP_REGISTRY="$P/registry" daemon -f -p "$P/limend.pid" -o "$P/limend.log" -u _limen "$P/limend"
for i in $(seq 30); do [ -S "$P/limen/limend.sock" ] && break; sleep 1; done
test -S "$P/limen/limend.sock"
chmod 666 "$P/limen/limend.sock"
"$P/prelumctl" --socket "$P/print.sock" add "ipps://1.2.3.2:$port/ipp/print"
daemon -f -p "$P/sheet.pid" -o "$P/sheet.log" -u _prelum-sheet env HOME=/nonexistent LIMEN_SOCKET="$P/limen/limend.sock" FRESCO_SOCKET="$P/fresco.sock" PRELUM_SOCKET="$P/print.sock" PRELUM_SHEET_ACTIVATION="$P/act.sock" PRELUM_SHEET_NEARBY=0 "$P/atrium-prelum-sheet"

test -f "$P/navigator-page"
mkdir -p bundle/bin
cp navigator-page bundle/bin/
cat > bundle/atrium.toml <<'TOML'
[app]
id = "org.test.prelum.quickhtml"
name = "Prelum test document viewer"
version = "1"
entry = "bin/navigator-page"

[resources]
memory = "512M"
cpu = 100
TOML
openssl dgst -sha256 -sign /root/of-key.pem -out bundle/atrium.toml.sig bundle/atrium.toml
opifex install bundle
portcullis policy grant --system org.test.prelum.quickhtml
pw usershow _viewer >/dev/null 2>&1 || {
  pw groupadd _viewer -g 50650
  pw useradd _viewer -u 50650 -g _viewer -d /nonexistent -s /usr/sbin/nologin
}
cat >> /etc/atrium/policy.toml <<'TOML'

[workers."org.test.prelum.quickhtml"]
callers = ["_limen"]
run_as = "_viewer"
concurrent = 4
TOML
PSOCK=/atrium/sockets/portcullis/portcullis.sock
! sockstat -u -l | grep -q "$PSOCK"
rm -f "$PSOCK"
env PORTCULLISD_FRESCO_ACTIVATION="$P/act.sock" daemon -f -p "$P/portcullis.pid" -o "$P/portcullis.log" /usr/local/bin/portcullisd --socket "$PSOCK"
for i in $(seq 30); do [ -S "$PSOCK" ] && break; sleep 1; done
test -S "$PSOCK"
M=/var/lib/atrium/apps/org.test.prelum.sheetsprint
mkdir -p "$M"
cat >"$M/atrium.toml" <<'TOML'
[app]
id = "org.test.prelum.sheetsprint"
name = "Prelum print fixture"
version = "1"
entry = "bin/scrinium"
[capabilities]
embeds = ["print", "doc-viewer", "picker"]
TOML

daemon -f -p "$P/scrinium.pid" -o "$P/app.log" -u prelusheets env HOME="$P/home" LIMEN_SOCKET="$P/limen/limend.sock" FRESCO_SOCKET="$P/fresco.sock" SCRINIUM_TRACE=1 SCRINIUM_PLACES="Proof=$P/files" "$P/scrinium" --no-picker
for i in $(seq 20); do grep -q 'entries=\[quicklook.html\]' app.log 2>/dev/null && break; sleep 1; done
grep -q 'entries=\[quicklook.html\]' app.log

inj() { printf '%s\n' "$*" | nc -N -U inject.sock; }
press() { inj "MOVE $1 $2"; sleep 1; inj 'BTN 1 1'; sleep 1; inj 'BTN 1 0'; }
key() { printf '%s\n' "KEY $1 1 $2" "KEY $1 0 $2" | nc -N -U inject.sock; }
wait_job() {
  for i in $(seq 35); do
    "$P/prelumctl" --socket "$P/print.sock" jobs > jobs.txt
    if grep -Eq "^ +$1 +printer .*Done" jobs.txt; then cat jobs.txt; return; fi
    sleep 1
  done
  cat jobs.txt sheet.log
  return 1
}
preview() {
  for i in $(seq 25); do
    n=$(grep -c "$1" sheet.log || :)
    if [ "$n" -ge "$2" ]; then sleep 3; return; fi
    sleep 1
  done
  cat sheet.log
  return 1
}
start_app() {
  mkdir "$1-home"
  chown prelusheets "$1-home"
  daemon -f -p "$P/$1.pid" -o "$P/$1.log" -u prelusheets env HOME="$P/$1-home" LIMEN_SOCKET="$P/limen/limend.sock" FRESCO_SOCKET="$P/fresco.sock" OFFICINA_CONVERT="$P/officina-convert" "$P/$1" "$P/$2"
  for i in $(seq 20); do grep -q 'converted' "$1.log" 2>/dev/null && break; sleep 1; done
  grep 'converted' "$1.log"
  sleep 4
}
(
  for i in $(seq 1800); do
    ps ax -o jid,comm | awk '/atrium-prelum-ipp/ && $1 > 0'
    sleep 0.1
  done
) > worker.log &
poll=$!
trap 'kill "$poll" 2>/dev/null || :' EXIT
press 300 130
key 44 0
for i in $(seq 20); do grep -q 'laid out' limend.log && break; sleep 1; done
grep 'laid out' limend.log
printf '<html><body>REPLACEMENT MUST NOT PRINT</body></html>\n' > replacement
mv replacement files/quicklook.html
press 915 112
preview 'previews: 4 of 4' 1
cp screen.png html-preview.png
press 779 534
wait_job 1
sleep 3
kill -TERM "$(cat scrinium.pid)"
sleep 2
start_app atrium-sheets sheets-proof.xlsx
key 19 1
preview 'previews: 1 of 1' 1
cp screen.png sheets-preview.png
press 849 553
wait_job 2
sleep 3
kill -TERM "$(cat atrium-sheets.pid)"
sleep 2
start_app atrium-slides slides-proof.pptx
press 286 23
sleep 2
press 332 145
preview 'previews: 2 of 2' 1
cp screen.png slides-preview.png
press 899 553
wait_job 3
sleep 3
kill -TERM "$(cat atrium-slides.pid)"
sleep 2
daemon -f -p "$P/mail.pid" -o "$P/mail.log" -u prelumail env HOME=/nonexistent LIMEN_SOCKET="$P/limen/limend.sock" FRESCO_SOCKET="$P/fresco.sock" "$P/prelum-print-fixture" "$P"
for i in $(seq 25); do grep -q 'count=1' mail.log 2>/dev/null && break; sleep 1; done
grep 'count=1' mail.log
press 999 24
sleep 2
press 330 179
for i in $(seq 20); do grep -q 'reading="Mail print fixture"' mail.log && break; sleep 1; done
grep 'reading="Mail print fixture"' mail.log
press 1234 34
preview 'previews: 3 of 3' 1
cp screen.png mail-preview.png
press 940 551
wait_job 4
sort -u worker.log
test "$(sort -u worker.log | wc -l | tr -d ' ')" -ge 4

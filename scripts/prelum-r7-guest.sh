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
# jaild's print worker policy names these entry points.
for worker in atrium-prelum-ipp atrium-prelum-render; do
    install -m 0555 "$P/$worker" "/usr/local/bin/$worker"
    test "$(sha256 -q "$P/$worker")" = "$(sha256 -q "/usr/local/bin/$worker")"
done
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
env LIMEN_DEFAULTS=doc-viewer=org.test.prelum.quickhtml,pdf-viewer=org.test.prelum.quickpdf LIMEN_SOCKET="$P/limen/limend.sock" LIMEN_FRESCOD_ACTIVATION="$P/act.sock" LIMEN_APP_REGISTRY="$P/registry" daemon -f -p "$P/limend.pid" -o "$P/limend.log" -u _limen "$P/limend"
for i in $(seq 30); do [ -S "$P/limen/limend.sock" ] && break; sleep 1; done
test -S "$P/limen/limend.sock"
chmod 666 "$P/limen/limend.sock"
"$P/prelumctl" --socket "$P/print.sock" add "ipps://1.2.3.2:$port/ipp/print"
daemon -f -p "$P/sheet.pid" -o "$P/sheet.log" -u _prelum-sheet env HOME=/nonexistent LIMEN_SOCKET="$P/limen/limend.sock" FRESCO_SOCKET="$P/fresco.sock" PRELUM_SOCKET="$P/print.sock" PRELUM_SHEET_ACTIVATION="$P/act.sock" PRELUM_SHEET_NEARBY=0 "$P/atrium-prelum-sheet"

install_viewer() {
    viewer_id=$1
    viewer_exec=$2
    mkdir -p "$viewer_id/bin"
    cp "$viewer_exec" "$viewer_id/bin/"
    cat >"$viewer_id/atrium.toml" <<TOML
[app]
id = "$viewer_id"
name = "Prelum test viewer"
version = "1"
entry = "bin/$viewer_exec"

[resources]
memory = "512M"
cpu = 100
TOML
    openssl dgst -sha256 -sign /root/of-key.pem -out "$viewer_id/atrium.toml.sig" "$viewer_id/atrium.toml"
    opifex install "$viewer_id"
    portcullis policy grant --system "$viewer_id"
}
install_viewer org.test.prelum.quickhtml navigator-page
install_viewer org.test.prelum.quickpdf atrium-pdf
pw usershow _viewer >/dev/null 2>&1 || {
  pw groupadd _viewer -g 50650
  pw useradd _viewer -u 50650 -g _viewer -d /nonexistent -s /usr/sbin/nologin
}
cat >> /etc/atrium/policy.toml <<'TOML'

[workers."org.test.prelum.quickhtml"]
callers = ["_limen"]
run_as = "_viewer"
concurrent = 4

[workers."org.test.prelum.quickpdf"]
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
embeds = ["print", "doc-viewer", "pdf-viewer", "picker"]
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
submit_job() {
  sample_offset=$(wc -l < worker.log)
  press "$2" "$3"
  wait_job "$1"
  if ! awk -v offset="$sample_offset" -v job="$1" '
    NR <= offset { seen[$1] = 1; next }
    !seen[$1]++ {
      printf "job %d: observed IPP worker in jail %d\n", job, $1
      found = 1
    }
    END { if (!found) exit 1 }
  ' worker.log >>worker-jobs.log; then
    echo "No new jailed IPP worker observed after submitting job $1" >&2
    return 1
  fi
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
  for i in $(seq 3600); do
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
submit_job 1 779 534
sleep 3
kill -TERM "$(cat scrinium.pid)"
sleep 2
start_app atrium-sheets sheets-proof.xlsx
key 19 1
preview 'previews: 1 of 1' 1
cp screen.png sheets-preview.png
submit_job 2 849 553
sleep 3
kill -TERM "$(cat atrium-sheets.pid)"
sleep 2
start_app atrium-slides slides-proof.pptx
press 286 23
sleep 2
press 332 145
preview 'previews: 2 of 2' 1
cp screen.png slides-hidden-off-preview.png
submit_job 3 899 553
sleep 3
# Toggle Include Hidden Slides in the Print menu and pick the same handout.
press 286 23
press 332 266
press 286 23
press 332 145
preview 'previews: 2 of 2' 2
# Select Letter in the print sheet after opening the A4 handout preview.
press 885 306
sleep 7
cp screen.png slides-hidden-on-letter-preview.png
submit_job 4 899 553
sleep 3
# Keep Include Hidden Slides enabled for speaker notes.
press 286 23
press 332 90
preview 'previews: 8 of 10' 1
cp screen.png slides-notes-hidden-on-preview.png
submit_job 5 899 553
sleep 3
# Turn Include Hidden Slides off and print the same deck's notes again.
press 286 23
press 332 266
press 286 23
press 332 90
preview 'previews: 8 of 9' 1
cp screen.png slides-notes-hidden-off-preview.png
submit_job 6 899 553
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
"$P/prelumctl" --socket "$P/print.sock" jobs >mail-before-cancel.txt
press 816 552
for i in 1 2 3; do
  sleep 1
  "$P/prelumctl" --socket "$P/print.sock" jobs >mail-after-cancel.txt
  diff -u mail-before-cancel.txt mail-after-cancel.txt
done
echo "Mail: cancelling the print preview queued no job"
press 1234 34
preview 'previews: 3 of 3' 2
cp screen.png mail-preview.png
submit_job 7 940 551
sleep 3
kill -TERM "$(cat mail.pid)"
sleep 2
daemon -f -p "$P/scrinium-text.pid" -o "$P/text-app.log" -u prelusheets env HOME="$P/home" LIMEN_SOCKET="$P/limen/limend.sock" FRESCO_SOCKET="$P/fresco.sock" SCRINIUM_TRACE=1 SCRINIUM_PLACES="Proof=$P/text-files" "$P/scrinium" --no-picker
for i in $(seq 20); do grep -q 'entries=\[quicklook-proof.txt\]' text-app.log 2>/dev/null && break; sleep 1; done
grep -q 'entries=\[quicklook-proof.txt\]' text-app.log
press 300 130
key 44 0
for i in $(seq 20); do grep -q 'look=quicklook-proof.txt:text:' text-app.log && break; sleep 1; done
grep 'look=quicklook-proof.txt:text:' text-app.log
printf 'REPLACEMENT MUST NOT PRINT\n' >text-replacement
mv text-replacement text-files/quicklook-proof.txt
press 915 112
preview 'previews: 5 of 5' 1
cp screen.png text-preview.png
submit_job 8 779 534
sleep 3
kill -TERM "$(cat scrinium-text.pid)"
sleep 2
daemon -f -p "$P/scrinium-image.pid" -o "$P/image-app.log" -u prelusheets env HOME="$P/home" LIMEN_SOCKET="$P/limen/limend.sock" FRESCO_SOCKET="$P/fresco.sock" SCRINIUM_TRACE=1 SCRINIUM_PLACES="Proof=$P/image-files" "$P/scrinium" --no-picker
for i in $(seq 20); do grep -q 'entries=\[quicklook-picture.png\]' image-app.log 2>/dev/null && break; sleep 1; done
grep -q 'entries=\[quicklook-picture.png\]' image-app.log
press 300 130
key 44 0
for i in $(seq 20); do grep -q 'picture 120x60' limend.log && break; sleep 1; done
grep 'picture 120x60' limend.log
press 915 112
preview 'previews: 1 of 1' 2
cp screen.png image-preview.png
submit_job 9 779 534
sleep 3
kill -TERM "$(cat scrinium-image.pid)"
sleep 2
daemon -f -p "$P/scrinium-pdf.pid" -o "$P/pdf-app.log" -u prelusheets env HOME="$P/home" LIMEN_SOCKET="$P/limen/limend.sock" FRESCO_SOCKET="$P/fresco.sock" SCRINIUM_TRACE=1 SCRINIUM_PLACES="Proof=$P/pdf-files" "$P/scrinium" --no-picker
for i in $(seq 20); do grep -q 'entries=\[quicklook.pdf\]' pdf-app.log 2>/dev/null && break; sleep 1; done
grep -q 'entries=\[quicklook.pdf\]' pdf-app.log
press 300 130
key 44 0
for i in $(seq 20); do jls -n name | grep -q quickpdf && break; sleep 1; done
jls -n name | grep quickpdf
mv replacement.pdf pdf-files/quicklook.pdf
press 915 112
preview 'previews: 2 of 2' 3
cp screen.png pdf-preview.png
submit_job 10 779 534
sleep 3
# Focus the PDF viewer and use its print shortcut on the retained preview.
press 748 304
key 19 1
preview 'previews: 2 of 2' 4
cp screen.png pdf-shortcut-preview.png
submit_job 11 779 534
cat worker-jobs.log

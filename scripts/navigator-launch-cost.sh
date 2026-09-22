#!/bin/sh
# Guest (root): per-document jail launch cost — navigator-backend.md §4.7c (M3, §10 q1).
# Stage release builds of portcullis, portcullisd, navigator-worker, jailed_corpus in /root/jlcost;
# recordings in /root/e2e-rec; broker user navtest; signing key /root/e2e-key.pem (the E2E sets these up).
# Arms interleaved A B C × RUNS so drift lands on every arm equally.
#   A = unconfined worker process (baseline: spawn + parse, no jail)
#   B = portcullis exec --user    (the SAME jaild one-shot lane, called directly
#                                  by root: no daemon hop — isolates its cost)
#   C = portcullis exec --daemon  (the same lane, run by portcullisd for uid navtest)
# Dead-arm check: jid delta per run — A must create 0 jails, B/C one per doc.
set -u
R=/root/jlcost; W=/home/navtest/jlcost; SOCK=/tmp/pd-jlcost.sock
APP=org.atrium.navigator.worker; RUNS=${RUNS:-3}
AS="su -m navtest -c"
nextjid() { j=$(jail -i -c name=jlprobe path=/ persist); jail -r jlprobe >/dev/null 2>&1; echo $j; }

# Release worker, installed signed, exactly as the E2E installs the debug one.
rm -rf /root/jl-bundle && mkdir -p /root/jl-bundle/bin
cp $R/navigator-worker /root/jl-bundle/bin/
printf '[app]\nid = "%s"\nname = "Navigator document worker"\nversion = "1"\nentry = "bin/navigator-worker"\n' $APP > /root/jl-bundle/atrium.toml
openssl dgst -sha256 -sign /root/e2e-key.pem -out /root/jl-bundle/atrium.toml.sig /root/jl-bundle/atrium.toml
/root/navigator-e2e/opifex install /root/jl-bundle >/dev/null 2>&1 || { echo "install failed"; exit 1; }
cmp -s $R/navigator-worker /var/lib/atrium/apps/$APP/bin/navigator-worker || { echo "installed worker is NOT the release build"; exit 1; }

rm -rf $W && mkdir -p $W && cp $R/jailed_corpus $R/portcullis $R/navigator-worker $W/ && cp -R /root/e2e-rec $W/rec && chown -R navtest $W

pkill -f "portcullisd --socket $SOCK" >/dev/null 2>&1; rm -f $SOCK
$R/portcullisd --socket $SOCK > /root/jlcost-pd.log 2>&1 &
sleep 2; pgrep -f "portcullisd --socket $SOCK" >/dev/null || { echo "portcullisd failed"; cat /root/jlcost-pd.log; exit 1; }

# The direct lane must refuse an unprivileged caller with the lane's reason.
r=$($AS "$W/portcullis exec --user navtest --instance x $APP </dev/null 2>&1")
echo "$r" | grep -q 'created by root' && echo "direct lane, uid navtest: refused with the reason" || echo "direct lane, uid navtest: WRONG: $r"
echo "kernel=$(sysctl -n kern.ident) sched=$(sysctl -n kern.sched.name) root=$(mount -p | awk '$2=="/"{print $3}') ncpu=$(sysctl -n hw.ncpu) docs=$(ls $W/rec/*.json | wc -l | tr -d ' ')"
for i in $(seq 1 $RUNS); do
  for arm in A B C; do
    case $arm in
      A) cmd="$W/jailed_corpus $W/rec $W/navigator-worker" ;;
      B) cmd="$W/jailed_corpus $W/rec $APP $W/portcullis exec --user navtest --instance '{instance}'" ;;
      C) cmd="PORTCULLIS_SOCKET=$SOCK $W/jailed_corpus $W/rec $APP $W/portcullis exec --daemon --instance '{instance}'" ;;
    esac
    j0=$(nextjid)
    if [ $arm = B ]; then out=$(sh -c "cd $W && $cmd 2>/dev/null")
    else out=$($AS "cd $W && $cmd 2>/dev/null"); fi
    j1=$(nextjid)
    ok=$(echo "$out" | grep -c '^OK$')
    echo "run=$i arm=$arm ok=$ok jails=$((j1 - j0 - 1)) $(echo "$out" | grep -E 'sessions,' )"
    echo "$out" | grep '_ms ' | sed "s/^/run=$i arm=$arm /"
  done
done
pkill -f "portcullisd --socket $SOCK"; rm -f $SOCK
echo "leaks: jails=$(jls -d name | grep -cE 'navigator[-_]worker') mounts=$(mount -p | grep -cE 'navigator[-_]worker')"

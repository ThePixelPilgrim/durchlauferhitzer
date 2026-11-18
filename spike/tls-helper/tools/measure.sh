#!/bin/bash
# measure.sh LABEL THREADS SECS -- server args...
# Starts the server, runs loadclient, prints handshakes and server-side CPU
# (server + spawner incl. reaped helpers + keyd) per handshake.
label=$1; threads=$2; secs=$3; shift 4
cd /srv/spike
./stop.sh
hs=""
if [ -n "$HSPAWN" ]; then setpriv --reuid 2002 --regid 2002 --clear-groups bin/server hspawn --helper /srv/spike/bin/durchlauferhitzer-tls --chain pki/cert.der --pool ${HSPAWN_POOL:-0} > logs/$label.hspawn.log 2>&1 & hs=$!; sleep 0.3; fi
bin/server serve "$@" > logs/$label.server.log 2>&1 &
sp=$!
sleep 0.6
pids="$sp $(pgrep -P $sp) $(pgrep -x keyd) $hs"
cpu() { local t=0; for p in $pids; do read -r -a f < /proc/$p/stat; t=$((t + f[13] + f[14] + f[15] + f[16])); done; echo $t; }
c0=$(cpu)
out=$(bin/loadclient 127.0.0.1:8443 pki/ca.der $threads $secs)
sleep 1.5
c1=$(cpu)
ok=$(echo "$out" | head -1 | sed 's/.*ok=\([0-9]*\).*/\1/')
fail=$(echo "$out" | head -1 | sed 's/.* fail=\([0-9]*\).*/\1/')
rate=$(echo "$out" | head -1 | sed 's/.*rate=\([0-9]*\).*/\1/')
lat=$(echo "$out" | head -1 | sed 's/.*lat_ms //')
ticks=$((c1-c0))
cl=$(echo "$out" | grep client_cpu | sed "s/.*per_conn=//")
echo "$label ok=$ok fail=$fail rate=${rate}/s server_cpu_ticks=$ticks cpu_us_per_hs=$(( ticks * 10000 / (ok>0?ok:1) )) $lat client_cpu_us_per_conn=$cl"

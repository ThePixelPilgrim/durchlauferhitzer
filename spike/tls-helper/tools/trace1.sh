#!/bin/bash
# trace1.sh HELPERBIN OUTDIR [extra env]: strace one helper end to end (per-pid files)
b=$1; out=$2; shift 2
cd /srv/spike; ./stop.sh; rm -rf $out; mkdir -p $out
env "$@" bin/server serve --mode spawn --port 8443 --helper /srv/spike/bin/$b --chain pki/cert.der > $out/server.log 2>&1 &
sp=$!; sleep 0.5
SP=$(pgrep -P $sp)
strace -f -ff -qq -e signal=none -o $out/t -p $SP &
st=$!; sleep 0.5
bin/loadclient 127.0.0.1:8443 pki/ca.der 1 0.001 > $out/client.txt
sleep 0.5; kill $st; sleep 0.3
H=$(grep -l '^execve' $out/t.* | head -1); echo "helper trace: $H"
awk '/^execve/{p="1-startup"} /^recvmsg\(3,/{p="2-handshake"} {n=$0; sub(/\(.*/,"",n); if(p=="") p="0-after-fork(spawner code)"; print p" "n}' $H | sort | uniq -c | awk '{print $2, $3, $1}'

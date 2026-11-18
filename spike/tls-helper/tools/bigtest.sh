#!/bin/bash
# bigtest.sh LABEL MODE URL [curl args]
label=$1; mode=$2; url=$3; shift 3
cd /srv/spike; ./stop.sh
bin/server serve --mode $mode --port 8443 --helper /srv/spike/bin/helper-musl --chain pki/cert.der --big /srv/spike/big.bin > logs/big-$label.log 2>&1 &
sp=$!; sleep 0.5
cpu() { local t=0; for p in $sp $(pgrep -P $sp); do read -r -a f < /proc/$p/stat; t=$((t + f[13] + f[14] + f[15] + f[16])); done; echo $t; }
for r in 1 2 3; do
c0=$(cpu)
TIMEFORMAT="curl_user=%U curl_sys=%S"
t0=$(date +%s.%N)
out=$( { time curl -sS $(for i in $(seq 10); do echo "-o /dev/null $url"; done) --cacert pki/ca.pem "$@" -w "%{num_connects} " ; } 2>&1 )
t1=$(date +%s.%N)
sleep 0.3; c1=$(cpu)
tt=$(echo "$t1-$t0" | bc)
echo "$label run$r 1000MiB conns=$(echo $out | grep -o '^[0-9 ]*' | tr -d ' ') MiB/s=$(echo "scale=0; 1000/$tt" | bc) wall=${tt}s server_cpu=$(echo "scale=2; ($c1-$c0)/100" | bc)s $(echo "$out" | grep -o 'curl_user.*')"
done

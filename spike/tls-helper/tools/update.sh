#!/bin/bash
# update.sh LABEL MODE(spawn|pool) METHOD(rename|cp) FLAVOR(musl|glibc)
label=$1; mode=$2; method=$3; fl=$4
cd /srv/spike; ./stop.sh
cp bin/helper-$fl-v1 bin/live-helper
bin/server serve --mode $mode --pool $([ $mode = pool ] && echo 8 || echo 0) --port 8443 --helper /srv/spike/bin/live-helper --chain pki/cert.der > logs/upd-$label.log 2>&1 &
sleep 0.6
t0=$(date +%s.%N)
bin/loadclient 127.0.0.1:8443 pki/ca.der 4 6 > logs/upd-$label.client.txt &
cl=$!
sleep 2
t1=$(date +%s.%N)
tries=0
if [ $method = rename ]; then
  cp bin/helper-$fl-v2 bin/live-helper.new && mv -f bin/live-helper.new bin/live-helper; tries=1
else
  end=$(( $(date +%s) + 8 ))
  while true; do tries=$((tries+1)); cp bin/helper-$fl-v2 bin/live-helper 2>>logs/upd-$label.cperr && { ok=1; break; }; [ $(date +%s) -ge $end ] && { ok=0; break; }; done
  echo "cp succeeded=$ok" >> logs/upd-$label.cperr
fi
t2=$(date +%s.%N)
wait $cl
echo "== $label: replace started at $(echo "$t1-$t0" | bc)s, took $(echo "($t2-$t1)*1000" | bc)ms, cp attempts=$tries, ETXTBSY lines=$(grep -c 'Text file busy' logs/upd-$label.cperr 2>/dev/null)"
cat logs/upd-$label.client.txt | grep -vE "^client_cpu"
grep -E "signal|status|failed" logs/upd-$label.log | cut -c1-100 | sort | uniq -c | sort -rn | head -5

#!/usr/bin/env bash
# netbench.sh <mbps> <rtt_ms> [secs] [w h x y] : selkies vs rs through a shaped link (SERVERS="rs selkies")
MBPS=$1; RTT=$2; SECS=${3:-20}; W=${4:-1920}; H=${5:-1080}; X=${6:-300}; Y=${7:-150}
cd "$(dirname "$(readlink -f "$0")")"
for name in ${SERVERS:-selkies rs}; do
  if [ $name = rs ]; then port=6902; sport=7902; else port=6901; sport=7901; fi
  ./py.sh shaper.py $sport localhost:$port $MBPS $RTT >/dev/null 2>&1 &
  sleep 3
  (DISPLAY=:1 ./loadgen $W $H 60 $((SECS+12)) $X $Y >/dev/null 2>&1 &)
  echo -n "[$name $MBPS Mbps, ${RTT}ms rtt] "
  ./py.sh nclient.py $name localhost:$sport $SECS $X $Y 2>/dev/null | tail -1
  pkill -f "shaper.py $sport"; sleep 4
done

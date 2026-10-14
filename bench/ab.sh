#!/usr/bin/env bash
# ab.sh <workload> <reps> [servers] : quick A/B on display :2 (rs=6902, selkies=6912), LAN
cd "$(dirname "$(readlink -f "$0")")"
wl=$1; reps=${2:-3}
for i in $(seq 1 $reps); do for name in ${3:-rs selkies}; do
  [ $name = rs ] && port=6902 || port=6912
  case $wl in
    scroll) mx=96; my=265; (BENCH_DISPLAY=:2 node scrolltarget.mjs 30 >/dev/null 2>&1 &) ;;
    anim1080) mx=300; my=150; (DISPLAY=:2 ./loadgen 1920 1080 60 28 300 150 >/dev/null 2>&1 &) ;;
    animfull) mx=0; my=0; (DISPLAY=:2 ./loadgen 2560 1600 60 28 0 0 >/dev/null 2>&1 &) ;;
  esac
  sleep 4
  ./py.sh nclient.py $name localhost:$port 15 $mx $my 2>/dev/null | python3 -c "import json,sys; d=json.loads(sys.stdin.read()); print('$wl $name', 'fps', d['uniqueFps']['avg'], 'min', d['uniqueFps']['min'], 'lat p50/p90/p99', d['latencyMs']['p50'], d['latencyMs']['p90'], d['latencyMs']['p99'], 'Mbps', d['mbps'])"
  sleep 10
done; done

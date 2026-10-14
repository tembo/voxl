#!/usr/bin/env bash
# suite.sh : selkies (python) vs selkies-rs across configs, workloads and links. Appends JSON lines to $OUT.
cd "$(dirname "$(readlink -f "$0")")"
OUT=${OUT:-/tmp/suite.jsonl}
SECS=${SECS:-20}
export BD=${BD:-:2} BENCH_DISPLAY=${BD:-:2}
run_one() { # config server port workload net
  local cfg=$1 srv=$2 port=$3 wl=$4 net=$5 mbps rtt mx my target=$3
  case $wl in
    scroll) mx=96; my=265; (node scrolltarget.mjs $((SECS+14)) >/dev/null 2>&1 &) ;;
    anim1080) mx=300; my=150; (DISPLAY=$BD ./loadgen 1920 1080 60 $((SECS+12)) 300 150 >/dev/null 2>&1 &) ;;
    animfull) mx=0; my=0; (DISPLAY=$BD ./loadgen 2560 1600 60 $((SECS+12)) 0 0 >/dev/null 2>&1 &) ;;
  esac
  if [ "$net" != "lan" ]; then
    mbps=${net%/*}; rtt=${net#*/}; target=$((port+1000))
    ./py.sh shaper.py $target localhost:$port $mbps $rtt >/dev/null 2>&1 &
  fi
  sleep 4
  res=$(./py.sh nclient.py $srv localhost:$target $SECS $mx $my 2>/dev/null | tail -1)
  [ "$net" != "lan" ] && pkill -f "shaper.py $target"
  echo "{\"config\":\"$cfg\",\"workload\":\"$wl\",\"net\":\"$net\",\"r\":${res:-null}}" | tee -a $OUT
  sleep 9
}
for cfg in ${CONFIGS:-py_picom py_nopicom rs_nopicom}; do
  case $cfg in py_picom) /tmp/picom2-start.sh ;; *) /tmp/picom2-stop.sh ;; esac
  sleep 2
  case $cfg in rs*) srv=rs; port=6902 ;; *) srv=selkies; port=6912 ;; esac
  for wl in ${WORKLOADS:-scroll anim1080 animfull}; do
    for net in ${NETS:-lan 10/40 5/60}; do
      [ $wl = animfull ] && [ $net != lan ] && continue
      run_one $cfg $srv $port $wl $net
    done
  done
done
/tmp/picom2-stop.sh

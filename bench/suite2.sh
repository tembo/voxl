#!/usr/bin/env bash
# suite2.sh : noVNC and Neko through the README workloads and links, with native clients
# (kclient.py for noVNC's x11vnc, nkclient.py for Neko's WebRTC). Bench display :2.
# Configs: novnc (env/novnc.sh), neko (Neko's defaults), neko_tuned (env/neko-tuned.sh).
# Links: TCP through shaper.py for noVNC; for Neko (UDP media) tc netem on its own
# container network, same rate, RTT and deep queue (see env/neko.sh).
cd "$(dirname "$(readlink -f "$0")")"
OUT=${OUT:-/tmp/suite2.jsonl}
SECS=${SECS:-20}
BD=${BENCH_DISPLAY:-:2}
NEKO_IP=${NEKO_IP:-10.0.2.15}
run_one() { # config workload net
  local cfg=$1 wl=$2 net=$3 mx my res mbps rtt
  case $wl in
    scroll) mx=96; my=265; (BENCH_DISPLAY=$BD node scrolltarget.mjs $((SECS+14)) >/dev/null 2>&1 &) ;;
    anim1080) mx=300; my=150; (DISPLAY=$BD ./loadgen 1920 1080 60 $((SECS+12)) 300 150 >/dev/null 2>&1 &) ;;
    animfull) mx=0; my=0; (DISPLAY=$BD ./loadgen 2560 1600 60 $((SECS+12)) 0 0 >/dev/null 2>&1 &) ;;
  esac
  mbps=${net%/*}; rtt=${net#*/}
  case $cfg in
    novnc)
      local port=6932
      [ "$net" != lan ] && { port=7932; ./py.sh shaper.py 7932 localhost:6932 $mbps $rtt >/dev/null 2>&1 & }
      sleep 4
      res=$(./py.sh kclient.py localhost:$port $SECS $mx $my 2>/dev/null | tail -1)
      [ "$net" != lan ] && env/killpat.sh "shaper.py 7932" >/dev/null 2>&1 ;;
    neko*)
      # Restart Neko for each link: shaped runs get their own network with tc netem.
      local addr starter=env/neko.sh
      [ $cfg = neko_tuned ] && starter=env/neko-tuned.sh
      if [ "$net" = lan ]; then addr=$(NEKO_SHAPE= $starter 2>/dev/null); else addr=$(NEKO_SHAPE=$net $starter 2>/dev/null); fi
      sleep 5
      res=$(timeout $((SECS+60)) /tmp/rtcpy.sh nkclient.py $addr $SECS $mx $my 2>/dev/null | tail -1) ;;
  esac
  echo "{\"config\":\"$cfg\",\"workload\":\"$wl\",\"net\":\"$net\",\"r\":${res:-null}}" | tee -a $OUT
  sleep 12
}
for cfg in ${CONFIGS:-novnc neko neko_tuned}; do
  for wl in ${WORKLOADS:-scroll anim1080 animfull}; do
    for net in ${NETS:-lan 10/40 5/60}; do
      [ $wl = animfull ] && [ $net != lan ] && continue
      run_one $cfg $wl $net
    done
  done
done

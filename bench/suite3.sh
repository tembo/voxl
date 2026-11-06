#!/usr/bin/env bash
# suite3.sh : Sunshine, streamed to Moonlight (mlclient.py), through the README workloads
# and links. Configs: sunshine (Sunshine's defaults with its software encoder, the only one
# without a GPU, at Moonlight's default bitrate), sunshine_tuned (x264 ultrafast on 4
# threads at 30 Mbps, like voxl). Moonlight doesn't adapt its bitrate, so on the slow links
# both get 80% of the link, as you'd set it by hand. Links: tc netem on Sunshine's own
# container network (env/sunshine.sh), the same rate, RTT and deep queue as shaper.py.
# Sunshine restarts for every run; pair mlclient.py's Moonlight with it first (see
# env/sunshine.sh). REPS runs each test that many times.
cd "$(dirname "$(readlink -f "$0")")"
OUT=${OUT:-/tmp/suite3.jsonl}
SECS=${SECS:-20}
BD=${BENCH_DISPLAY:-:2}
run_one() { # config workload net
  local cfg=$1 wl=$2 net=$3 mx my addr shape= opts= br=
  [ $cfg = sunshine_tuned ] && { opts='sw_preset=ultrafast min_threads=4'; br=30000; }
  [ "$net" != lan ] && { shape=$net; br=$((${net%/*} * 800)); }
  # Sunshine first: a shaped container takes a while to come up.
  addr=$(SUNSHINE_SHAPE=$shape SUNSHINE_OPTS=$opts env/sunshine.sh 2>/dev/null) || return 1
  case $wl in
    scroll) mx=96; my=265; (BENCH_DISPLAY=$BD node scrolltarget.mjs $((SECS+14)) >/dev/null 2>&1 &) ;;
    anim1080) mx=300; my=150; (DISPLAY=$BD ./loadgen 1920 1080 60 $((SECS+12)) 300 150 >/dev/null 2>&1 &) ;;
    animfull) mx=0; my=0; (DISPLAY=$BD ./loadgen 2560 1600 60 $((SECS+12)) 0 0 >/dev/null 2>&1 &) ;;
  esac
  sleep 4
  res=$(ML_BITRATE=$br timeout $((SECS+60)) ./py.sh mlclient.py $addr $SECS $mx $my 2>/dev/null | tail -1)
  sleep 12
  # No frames: Sunshine died mid-run (see env/sunshine.sh); the caller retries once.
  case $res in '' | *'"avg": 0.0'*) return 1 ;; esac
  echo "{\"config\":\"$cfg\",\"workload\":\"$wl\",\"net\":\"$net\",\"r\":$res}" | tee -a $OUT
}
for rep in $(seq ${REPS:-1}); do
  for cfg in ${CONFIGS:-sunshine sunshine_tuned}; do
    for wl in ${WORKLOADS:-scroll anim1080 animfull}; do
      for net in ${NETS:-lan 10/40 5/60}; do
        [ $wl = animfull ] && [ $net != lan ] && continue
        run_one $cfg $wl $net || run_one $cfg $wl $net || echo "{\"config\":\"$cfg\",\"workload\":\"$wl\",\"net\":\"$net\",\"r\":null}" | tee -a $OUT
      done
    done
  done
done
docker rm -f sunshine-bench >/dev/null 2>&1
kill "$(cat "${SUNSHINE_DIR:-/tmp/voxl-bench-sunshine}/pid")" 2>/dev/null
true

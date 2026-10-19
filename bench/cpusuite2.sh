#!/usr/bin/env bash
# cpusuite2.sh : server CPU (X server + streamer) for noVNC and Neko with one native
# client connected, per workload, as cpusuite.sh. noVNC = x11vnc + websockify + Xvfb;
# Neko = the neko server (in its container) + Xvfb.
cd "$(dirname "$(readlink -f "$0")")"
OUT=${OUT:-/tmp/cpusuite2.jsonl}
BD=${BENCH_DISPLAY:-:2}
pid_of() { pgrep -f -- "$1" | head -1; }
cores() {
  local a=0 b=0 p
  for p in "$@"; do a=$((a + $(awk '{print $14+$15}' /proc/$p/stat))); done
  sleep 10
  for p in "$@"; do b=$((b + $(awk '{print $14+$15}' /proc/$p/stat))); done
  python3 -c "print(f'{($b-$a)/100/10:.2f}')"
}
for srv in ${SERVERS:-novnc neko neko_tuned}; do
  case $srv in
    novnc) pids="$(pid_of 'rfbport 5902') $(pid_of '6932 localhost:5902') $(pid_of "Xvfb $BD ")"; cli="./py.sh kclient.py localhost:6932" ;;
    neko|neko_tuned)
      docker rm -f neko-bench >/dev/null 2>&1
      if [ $srv = neko ]; then env/neko.sh >/dev/null; else env/neko-tuned.sh >/dev/null; fi
      sleep 5
      pids="$(docker inspect -f '{{.State.Pid}}' neko-bench) $(pid_of "Xvfb $BD ")"; cli="/tmp/rtcpy.sh nkclient.py 127.0.0.1:6942" ;;
  esac
  for wl in ${WORKLOADS:-idle anim1080 scroll animfull}; do
    case $wl in
      scroll) (BENCH_DISPLAY=$BD node scrolltarget.mjs 34 >/dev/null 2>&1 &) ;;
      anim1080) (DISPLAY=$BD ./loadgen 1920 1080 60 30 300 150 >/dev/null 2>&1 &) ;;
      animfull) (DISPLAY=$BD ./loadgen 2560 1600 60 30 0 0 >/dev/null 2>&1 &) ;;
    esac
    sleep 4
    ($cli 22 >/dev/null 2>&1 &)
    sleep 10
    echo "{\"server\":\"$srv\",\"workload\":\"$wl\",\"cores\":$(cores $pids)}" | tee -a $OUT
    sleep 22
  done
done

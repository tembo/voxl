#!/usr/bin/env bash
# cpusuite.sh : server CPU (cores) with one native client connected, per workload.
# Counts the X server plus the streamer, since KasmVNC is both in one process (Xvnc):
#   voxl = voxl + Xvfb on :2, kasm = Xvnc on :5, selkies = selkies + Xvfb on :2.
# Start the servers as for suite.sh; no picom. Appends JSON lines to $OUT.
cd "$(dirname "$(readlink -f "$0")")"
OUT=${OUT:-/tmp/cpusuite.jsonl}
pid_of() { pgrep -f -- "$1" | head -1; }
cores() { # pids... : cores used over 10 s
  local a=0 b=0 p
  for p in "$@"; do a=$((a + $(awk '{print $14+$15}' /proc/$p/stat))); done
  sleep 10
  for p in "$@"; do b=$((b + $(awk '{print $14+$15}' /proc/$p/stat))); done
  python3 -c "print(f'{($b-$a)/100/10:.2f}')"
}
for srv in ${SERVERS:-voxl kasm}; do
  case $srv in
    voxl) BD=:2; pids="$(pid_of '--port=6902') $(pid_of 'Xvfb :2 ')"; cli="nclient.py voxl localhost:6902" ;;
    selkies) BD=:2; pids="$(pid_of '--port=6912') $(pid_of 'Xvfb :2 ')"; cli="nclient.py selkies localhost:6912" ;;
    kasm) BD=${KASM_DISPLAY:-:5}; pids="$(pid_of "Xvnc $BD ")"; cli="kclient.py localhost:6922" ;;
  esac
  export BENCH_DISPLAY=$BD
  for wl in ${WORKLOADS:-idle anim1080 scroll animfull}; do
    case $wl in
      scroll) (node scrolltarget.mjs 34 >/dev/null 2>&1 &) ;;
      anim1080) (DISPLAY=$BD ./loadgen 1920 1080 60 30 300 150 >/dev/null 2>&1 &) ;;
      animfull) (DISPLAY=$BD ./loadgen 2560 1600 60 30 0 0 >/dev/null 2>&1 &) ;;
    esac
    sleep 4
    (./py.sh $cli 22 >/dev/null 2>&1 &)
    sleep 10
    echo "{\"server\":\"$srv\",\"workload\":\"$wl\",\"cores\":$(cores $pids)}" | tee -a $OUT
    sleep 22
  done
done

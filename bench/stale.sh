#!/usr/bin/env bash
# stale.sh <voxl binary> [runs] (WINDOW=text|photo) : slow-link check that a viewer ends up with the right picture.
# On a 10 Mbps / 40 ms link (shaper.py), a detailed 2000x1600 window appears (a scene
# change the bitrate crushes) on a plain screen, and a small 520x200 animation starts 0.5 s later and keeps
# running. Once the window has been still for a few seconds, stalecheck.py compares the
# viewer's decoded picture of the window with the real screen, and reports the animation's
# frame rate and latency, second by second, while the window sharpens. Needs loadgen (see README)
# feh, xsetroot and an Xvfb on BENCH_DISPLAY (default :2); voxl is started on port 6902.
cd "$(dirname "$(readlink -f "$0")")"
BIN=${1:?voxl binary}
RUNS=${2:-1}
D=${BENCH_DISPLAY:-:2}
./py.sh stillpic.py /tmp/stale-window.png 2000 1600 ${WINDOW:-text}
for run in $(seq 1 "$RUNS"); do
  ./env/killpat.sh "--port=6902" >/dev/null 2>&1
  # Every run starts from the same plain screen (an earlier run or benchmark leaves its
  # last picture on the root window, which changes what the rate control starts from).
  DISPLAY=$D xsetroot -solid '#3a3a3a'
  DISPLAY=$D VOXL_DEBUG_STRIPES=${DEBUG:-} "$BIN" --port=6902 --enable-basic-auth=false --audio-enabled=false --max-kbps=30000 \
    >/tmp/stale-voxl.log 2>&1 &
  voxl=$!
  sleep 1.5
  ./py.sh shaper.py 7902 localhost:6902 10 40 >/dev/null 2>&1 &
  shaper=$!
  sleep 1
  MARKER=2020,700 ./py.sh stalecheck.py localhost:7902 localhost:6902 18 0,0,2000,1600 /tmp/stale-seen.png >/tmp/stale.json &
  check=$!
  # Wait for the viewer to be streaming, so the window is a scene change, not part of
  # the keyframe a new viewer starts with.
  for _ in $(seq 1 100); do
    curl -s localhost:6902/stats | grep -q '"video":true' && break
    sleep 0.1
  done
  sleep 1.5
  # The window: a still picture of loadgen's detailed text (feh, borderless).
  DISPLAY=$D feh --borderless --geometry 2000x1600+0+0 /tmp/stale-window.png >/dev/null 2>&1 &
  window=$!
  sleep 0.5
  DISPLAY=$D ./loadgen 520 200 60 20 2020 700 >/dev/null 2>&1 &
  anim=$!
  wait $check
  echo "run $run: $(cat /tmp/stale.json)"
  kill $window $anim $shaper $voxl 2>/dev/null
  wait $window $anim $shaper $voxl 2>/dev/null
  ./env/killpat.sh "shaper.py 7902" >/dev/null 2>&1
done

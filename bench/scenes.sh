#!/usr/bin/env bash
# scenes.sh <voxl binary> [mbps/rtt_ms] : scene-change stress with a viewer connected.
# Six large windows appear one after another (loadgen, 1 s each) and the root window is
# repainted with a new pattern between them, while a viewer (stalecheck.py) answers
# pings and acknowledges frames, over a link shaped by shaper.py if one is given (e.g.
# 10/40). Prints the encode thread's summary (stripelog.py); stale.sh checks the picture
# (xsetroot's fine patterns are beyond any lossy encoder, so they aren't compared here).
# Needs loadgen, xsetroot and an Xvfb on BENCH_DISPLAY (default :2); voxl is started on
# port 6902 with --max-kbps=30000.
cd "$(dirname "$(readlink -f "$0")")"
BIN=${1:?voxl binary}
NET=${2:-lan}
D=${BENCH_DISPLAY:-:2}
./env/killpat.sh "--port=6902" >/dev/null 2>&1
DISPLAY=$D VOXL_DEBUG_STRIPES=1 "$BIN" --port=6902 --enable-basic-auth=false --audio-enabled=false \
  --max-kbps=30000 >/tmp/scenes-voxl.log 2>&1 &
voxl=$!
sleep 1.5
port=6902
if [ "$NET" != lan ]; then
  ./py.sh shaper.py 7902 localhost:6902 "${NET%/*}" "${NET#*/}" >/dev/null 2>&1 &
  shaper=$!
  port=7902
  sleep 1
fi
./py.sh stalecheck.py localhost:$port localhost:6902 16 >/dev/null &
viewer=$!
for _ in $(seq 1 100); do
  curl -s localhost:6902/stats | grep -q '"video":true' && break
  sleep 0.1
done
sleep 1
for i in 1 2 3 4 5 6; do
  DISPLAY=$D ./loadgen $((1200 + i * 150)) $((700 + i * 80)) 60 1 $((i * 90)) $((i * 60)) >/dev/null 2>&1
  DISPLAY=$D xsetroot -mod $((i + 1)) $((i + 2)) -fg "#$(printf '%02x' $((i * 40)))2080" \
    -bg "#40$(printf '%02x' $((i * 30)))c0"
  sleep 0.3
done
wait $viewer
echo "encode thread: $(./py.sh stripelog.py /tmp/scenes-voxl.log)"
kill $voxl ${shaper:-} 2>/dev/null
wait $voxl ${shaper:-} 2>/dev/null
./env/killpat.sh "shaper.py 7902" >/dev/null 2>&1

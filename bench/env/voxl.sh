#!/usr/bin/env bash
# (Re)start voxl on the benchmark display, port 6902. Extra args are passed through.
cd "$(dirname "$(readlink -f "$0")")"
BIN=${VOXL_BIN:-../../target/release/voxl}
./killpat.sh "--port=6902" >/dev/null 2>&1
sleep 1
DISPLAY=${BENCH_DISPLAY:-:2} RUST_LOG=${RUST_LOG:-info} setsid nohup "$BIN" --port=6902 --framerate=60 --preset=ultrafast --max-kbps=30000 --audio-enabled=false "$@" > /tmp/voxl-bench.log 2>&1 < /dev/null &
for i in $(seq 1 50); do ss -ltn | grep -q ":6902 " && break; sleep 0.2; done

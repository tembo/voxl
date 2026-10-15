#!/usr/bin/env bash
# Production picom config on the benchmark display (for the "selkies today" baseline).
cd "$(dirname "$(readlink -f "$0")")"
DISPLAY=${BENCH_DISPLAY:-:2} setsid nohup picom --config "$PWD/picom.conf" --log-file /tmp/picom-bench.log >/dev/null 2>&1 < /dev/null &

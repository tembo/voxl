#!/usr/bin/env bash
# picom on the benchmark display. Defaults to the config Python selkies ran with
# (use-damage off); PICOM_CONF=picom-damage.conf with PICOM set to the monorepo's
# back-buffer-patched build matches the Computer preview today.
cd "$(dirname "$(readlink -f "$0")")"
DISPLAY=${BENCH_DISPLAY:-:2} setsid nohup "${PICOM:-picom}" --config "$PWD/${PICOM_CONF:-picom.conf}" --log-file /tmp/picom-bench.log >/dev/null 2>&1 < /dev/null &

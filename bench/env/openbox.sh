#!/usr/bin/env bash
# Window manager for the benchmark display, with the production X resources (Xft.dpi 144).
export DISPLAY=${BENCH_DISPLAY:-:2} HOME=/tmp/voxl-bench-home
mkdir -p "$HOME"
printf 'Xft.dpi: 144\nXft.antialias: 1\nXft.hinting: 1\nXcursor.size: 36\n' | xrdb -load -
xsetroot -solid '#1f2937' || true
exec openbox

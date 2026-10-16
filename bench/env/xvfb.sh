#!/usr/bin/env bash
# Isolated benchmark display, configured like the Computer preview (2560x1600 @ 144 DPI,
# -shmem so voxl can read the framebuffer directly).
D=${BENCH_DISPLAY:-:2}
# XVFB: path to Xvfb (use voxl's xvfb.nix build for smooth scrolling).
exec "${XVFB:-Xvfb}" $D -screen 0 2560x1600x24 -dpi 144 -shmem -ac -nolisten tcp -noreset

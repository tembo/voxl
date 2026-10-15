#!/usr/bin/env bash
# Python selkies on the benchmark display, port 6912, with the Tembo production flags.
# SELKIES_BIN: path to the selkies executable (nix/packages/selkies in the monorepo).
export DISPLAY=${BENCH_DISPLAY:-:2}
exec "${SELKIES_BIN:-selkies}" --addr=0.0.0.0 --port=6912 --enable-basic-auth=false --enable-clipboard=true --file-transfers=none --audio-enabled=false --microphone-enabled=false --ui-show-sidebar=false --framerate=60-60 --video-bitrate=18000-18000 --rate-control-mode=cbr --use-paint-over-quality=true --video-paintover-crf=18-18 --video-paintover-burst-frames=5-5 --keyframe-interval=2 --is-manual-resolution-mode=true --manual-width=2560 --manual-height=1600 --encoder=h264enc --video-streaming-mode=true --use-cpu=true --backpressure-queue-size=2 --enable-resize=false --enable-metrics-http=false

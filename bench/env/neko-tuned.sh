#!/usr/bin/env bash
# Neko tuned like voxl: 60 FPS H.264 from x264 ultrafast/zerolatency at up to 30 Mbps
# (Neko's default is VP8 at 25 FPS, ~2 Mbps).
cd "$(dirname "$(readlink -f "$0")")"
D=${BENCH_DISPLAY:-:2}
export NEKO_CODEC=h264
export NEKO_PIPELINE="ximagesrc display-name=$D show-pointer=false use-damage=false ! video/x-raw,framerate=60/1 ! videoconvert ! queue max-size-buffers=1 max-size-time=0 max-size-bytes=0 leaky=downstream ! video/x-raw,format=I420 ! x264enc threads=4 bitrate=30000 vbv-buf-capacity=100 key-int-max=600 byte-stream=true tune=zerolatency speed-preset=ultrafast ! video/x-h264,stream-format=byte-stream,profile=constrained-baseline ! appsink name=appsink"
exec ./neko.sh

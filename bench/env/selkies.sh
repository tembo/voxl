#!/usr/bin/env bash
# (Re)start selkies (github.com/selkies-project/selkies) from its base image, port 6912, on
# the image's XLibre Xvfb (display :3): selkies reads the screen in place only from its own
# X server. Both run in containers sharing the host's network, IPC and X sockets. x264 at
# 60 FPS, CBR from 30 Mbps with congestion control down to 2 Mbps, paint-over to CRF 16, like
# voxl. Extra args are passed through. SELKIES_IMAGE: the image (a local addons/base build
# works too). SELKIES_DISPLAY: the display. Its log is `docker logs selkies-bench`.
IMAGE=${SELKIES_IMAGE:-ghcr.io/selkies-project/selkies/base:main-debiantrixie}
D=${SELKIES_DISPLAY:-:3}
[ -e "/tmp/.X${D#:}-lock" ] && { echo "display $D is in use on the host; set SELKIES_DISPLAY" >&2; exit 1; }
# As the caller, so X clients' MIT-SHM segments (mode 0600) attach to its Xvfb.
U=(--user "$(id -u):$(id -g)" -e HOME=/tmp -e XDG_RUNTIME_DIR=/tmp)
docker rm -f selkies-bench selkies-bench-x >/dev/null 2>&1
mkdir -p -m 1777 /tmp/.X11-unix; rm -f "/tmp/.X11-unix/X${D#:}"
docker run -d --name selkies-bench-x "${U[@]}" --network host --ipc host -v /tmp/.X11-unix:/tmp/.X11-unix \
  --entrypoint /usr/bin/Xvfb "$IMAGE" $D -screen 0 2560x1600x24 -dpi 144 -ac -nolisten tcp -noreset -fakescreenfps 60 >/dev/null
for i in $(seq 1 50); do [ -S "/tmp/.X11-unix/X${D#:}" ] && break; sleep 0.2; done
[ -S "/tmp/.X11-unix/X${D#:}" ] || { echo "Xvfb $D did not start: docker logs selkies-bench-x" >&2; exit 1; }
docker run -d --name selkies-bench "${U[@]}" --network host --ipc host -v /tmp/.X11-unix:/tmp/.X11-unix \
  -e DISPLAY=$D --entrypoint /usr/local/bin/selkies "$IMAGE" \
  --addr=127.0.0.1 --port=6912 --mode=websockets --enable-dual-mode=false --enable-https=false --enable-basic-auth=false \
  --encoder=h264enc --use-cpu=true --video-fullcolor=false \
  --rate-control-mode=cbr --congestion-control=true --video-bitrate=30000,2000-30000 \
  --framerate=60-60 --keyframe-interval=0 --video-streaming-mode=false --video-paintover-crf=16 \
  --manual-width=2560 --manual-height=1600 --scaling-dpi=144 --enable-resize=false \
  --audio-enabled=false --microphone-enabled=false --gamepad-enabled=false --file-transfers=none \
  --printing-enabled=false --enable-sharing=false --enable-clipboard=true --ui-show-sidebar=false \
  --enable-metrics-http=false "$@" >/dev/null
for i in $(seq 1 100); do ss -ltn | grep -q ":6912 " && break; sleep 0.2; done
ss -ltn | grep -q ":6912 " || { echo "selkies did not start: docker logs selkies-bench" >&2; exit 1; }

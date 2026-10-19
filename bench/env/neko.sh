#!/usr/bin/env bash
# Neko (github.com/m1k1o/neko, official image) streaming the benchmark display over
# WebRTC on port 6942: the neko server only (not the image's own Xorg), sharing the
# display and XShm (IPC) with the other servers.
# NEKO_CODEC: vp8 (Neko's default) or h264. NEKO_SCREEN: size@fps.
# NEKO_SHAPE=<mbps>/<rtt_ms>: the container gets its own network and a sidecar shapes
# everything it sends with tc netem (rate, the whole RTT as delay, a deep queue like
# shaper.py's). WebRTC media is UDP, so shaper.py's TCP proxy can't carry it.
# Prints the address to connect to (host:port).
docker rm -f neko-bench >/dev/null 2>&1
if [ -n "$NEKO_SHAPE" ]; then
  docker network create --subnet 172.30.9.0/24 nekobench >/dev/null 2>&1
  net=(--network nekobench --ip 172.30.9.10); bind=0.0.0.0:6942; nat=(-e NEKO_WEBRTC_NAT1TO1=172.30.9.10)
else
  net=(--network host); bind=127.0.0.1:6942; nat=(-e NEKO_WEBRTC_NAT1TO1=${NEKO_IP:-10.0.2.15})
fi
docker run -d --name neko-bench "${net[@]}" --ipc host \
  -v /tmp/.X11-unix:/tmp/.X11-unix \
  -e NEKO_DESKTOP_DISPLAY=${BENCH_DISPLAY:-:2} -e NEKO_CAPTURE_VIDEO_DISPLAY=${BENCH_DISPLAY:-:2} \
  -e NEKO_DESKTOP_SCREEN=${NEKO_SCREEN:-2560x1600@60} -e NEKO_DESKTOP_INPUT_ENABLED=false \
  -e NEKO_CAPTURE_VIDEO_CODEC=${NEKO_CODEC:-vp8} \
  -e NEKO_CAPTURE_AUDIO_PIPELINE='audiotestsrc wave=silence is-live=true ! audioconvert ! opusenc ! appsink name=appsink' \
  -e NEKO_SERVER_BIND=$bind -e NEKO_SERVER_STATIC=/var/www \
  -e NEKO_WEBRTC_UDPMUX=59000 -e NEKO_WEBRTC_ICELITE=true "${nat[@]}" \
  -e NEKO_MEMBER_PROVIDER=noauth -e NEKO_SESSION_IMPLICIT_HOSTING=true \
  ${NEKO_PIPELINE:+-e "NEKO_CAPTURE_VIDEO_PIPELINE=$NEKO_PIPELINE"} \
  --entrypoint /usr/bin/neko ghcr.io/m1k1o/neko/base:latest serve >/dev/null
if [ -n "$NEKO_SHAPE" ]; then
  docker run --rm --network container:neko-bench --cap-add NET_ADMIN alpine:3.20 sh -c \
    "apk add -q iproute2 >/dev/null 2>&1 && tc qdisc add dev eth0 root netem rate ${NEKO_SHAPE%/*}mbit delay ${NEKO_SHAPE#*/}ms limit 1000000" >&2
  echo 172.30.9.10:6942
else
  echo 127.0.0.1:6942
fi

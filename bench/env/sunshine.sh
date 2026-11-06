#!/usr/bin/env bash
# Sunshine (github.com/LizardByte/Sunshine) streaming the benchmark display with its
# software (x264) encoder, the only one on a VM without a GPU; measured with mlclient.py.
# SUNSHINE: the sunshine binary (a Nix store path when SUNSHINE_SHAPE is set, since the
# container mounts /nix/store). SUNSHINE_DIR (default /tmp/voxl-bench-sunshine) keeps
# its config, certificates and pairings; SUNSHINE_OPTS adds name=value settings
# (suite3.sh's tuned run: "sw_preset=ultrafast min_threads=4").
# SUNSHINE_SHAPE=<mbps>/<rtt_ms>: like env/neko.sh, a container on its own network
# with tc netem shaping everything it sends (Moonlight's video is UDP, which
# shaper.py's TCP proxy can't carry).
# Pair once: HOME=$ML_HOME moonlight pair <addr> prints a PIN; give it to Sunshine with
#   curl -k -u bench:bench -H 'Content-Type: application/json' -d '{"pin":"<PIN>","name":"bench"}' https://localhost:47990/api/pin
# Prints the address to connect to.
dir=${SUNSHINE_DIR:-/tmp/voxl-bench-sunshine}
bin=${SUNSHINE:-$(command -v sunshine)}
mkdir -p "$dir/home"
if [ ! -f "$dir/sunshine.conf" ]; then
  cat >"$dir/sunshine.conf" <<EOF
capture = x11
encoder = software
origin_web_ui_allowed = lan
upnp = disabled
file_apps = $dir/apps.json
file_state = $dir/state.json
credentials_file = $dir/state.json
pkey = $dir/cakey.pem
cert = $dir/cacert.pem
log_path = $dir/sunshine.log
EOF
  echo '{"env": {}, "apps": [{"name": "Desktop"}]}' >"$dir/apps.json"
  HOME=$dir/home "$bin" "$dir/sunshine.conf" --creds bench bench >/dev/null 2>&1
fi
stop() {
  docker rm -f sunshine-bench >/dev/null 2>&1
  [ -f "$dir/pid" ] && kill "$(cat "$dir/pid")" 2>/dev/null && sleep 2
  rm -f "$dir/pid"
}
alive() {
  if [ -n "$SUNSHINE_SHAPE" ]; then [ "$(docker inspect -f '{{.State.Running}}' sunshine-bench 2>/dev/null)" = true ]
  else kill -0 "$(cat "$dir/pid" 2>/dev/null)" 2>/dev/null; fi
}
# shellcheck disable=SC2086
start() {
  if [ -n "$SUNSHINE_SHAPE" ]; then
    docker network create --subnet 172.30.10.0/24 sunshinebench >/dev/null 2>&1
    docker run -d --name sunshine-bench --network sunshinebench --ip 172.30.10.10 --ipc host \
      -v /nix/store:/nix/store:ro -v /tmp/.X11-unix:/tmp/.X11-unix -v "$dir:$dir" \
      -e DISPLAY=${BENCH_DISPLAY:-:2} -e HOME="$dir/home" \
      alpine:3.20 "$bin" "$dir/sunshine.conf" $SUNSHINE_OPTS >/dev/null
    addr=172.30.10.10
  else
    cd "$dir" || exit 1
    DISPLAY=${BENCH_DISPLAY:-:2} HOME=$dir/home "$bin" "$dir/sunshine.conf" $SUNSHINE_OPTS </dev/null >/dev/null 2>&1 &
    echo $! >"$dir/pid"
    cd - >/dev/null || exit 1
    addr=localhost
  fi
}
# Sunshine tries its encoders before it listens, and now and then crashes while it
# starts ("malloc(): unaligned tcache chunk detected"); wait until it answers, retrying.
for _ in 1 2 3; do
  stop
  start
  ok=
  for _ in $(seq 60); do
    alive || break
    curl -s -o /dev/null --max-time 2 "http://$addr:47989/serverinfo" && { ok=1; break; }
    sleep 0.5
  done
  [ -n "$ok" ] && break
done
[ -z "$ok" ] && { echo "sunshine didn't start" >&2; exit 1; }
if [ -n "$SUNSHINE_SHAPE" ]; then
  timeout 60 docker run --rm --network container:sunshine-bench --cap-add NET_ADMIN alpine:3.20 sh -c \
    "apk add -q iproute2 >/dev/null 2>&1 && tc qdisc add dev eth0 root netem rate ${SUNSHINE_SHAPE%/*}mbit delay ${SUNSHINE_SHAPE#*/}ms limit 1000000" >&2
fi
echo $addr

#!/usr/bin/env bash
# Restart the dev server on :6902 with extra args.
pkill -f "target/release/selkies-rs" ; while pgrep -f "target/release/selkies-rs" >/dev/null; do sleep 0.1; done
sleep 0.3
cd "$(dirname "$(readlink -f "$0")")/.."
DISPLAY=:1 RUST_LOG=${RUST_LOG:-info} nohup ./target/release/selkies-rs --port=6902 "$@" > /tmp/srs.log 2>&1 &
sleep 0.7

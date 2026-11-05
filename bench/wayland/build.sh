#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p build
proto=$(pkg-config --variable=pkgdatadir wayland-protocols)/stable/xdg-shell/xdg-shell.xml
wayland-scanner client-header "$proto" build/xdg-shell-client-protocol.h
wayland-scanner private-code "$proto" build/xdg-shell-protocol.c
cc -O2 -Wall -Wextra -Werror -Ibuild loadgen.c build/xdg-shell-protocol.c -o build/loadgen $(pkg-config --cflags --libs wayland-client x11 xext)

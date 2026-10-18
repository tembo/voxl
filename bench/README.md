# Benchmarks

Tools used to compare voxl with selkies and KasmVNC. Everything runs on a separate Xvfb display
(`:2` by default, `BENCH_DISPLAY` to change it), so a live desktop isn't disturbed.

## Setup

```sh
cc -O2 -o loadgen loadgen.c -lX11 -lXext      # 60 Hz animation with a frame/time marker
cc -O2 -o rootlat rootlat.c -lX11             # marker latency on the root window
env/xvfb.sh & sleep 1; env/openbox.sh &       # 2560x1600 @ 144 DPI, like production
env/voxl.sh                                   # voxl on :6902 (VOXL_BIN to override)
SELKIES_BIN=/path/to/selkies env/selkies.sh & # selkies on :6912
KASMVNC=/path/to/kasmvncserver XKB_DIR=/path/to/xkb env/kasmvnc.sh &  # KasmVNC (own X server) :5, port 6922
sleep 1; BENCH_DISPLAY=:5 env/openbox.sh &
```

`KASMVNC` is the monorepo's `nix/packages/kasmvnc` build; `XKB_DIR` is
`xkeyboard_config`'s `share/X11/xkb`. `env/kasmvnc.sh` passes the flags Tembo's
`kasmvnc.yaml` expands to.

`*.mjs` need Playwright (`PLAYWRIGHT_NODE_MODULES`) and Chromium (`CHROMIUM`).
`py.sh` runs Python with PyAV and websockets through nix-shell.

## Measuring

- `nclient.py voxl|selkies host:port secs [x y]`: native client (FFmpeg decode)
  that reads the marker at (x, y). Reports unique FPS, latency and Mbps.
- `kclient.py host:port secs [x y]`: native KasmVNC client. Speaks RFB over its
  WebSocket the way KasmVNC's web client does (same pixel format and encodings,
  including JPEG, WebP and CopyRect, continuous updates and fences), keeps a full
  framebuffer, decodes images in parallel and reads the marker after each update.
  Same JSON as `nclient.py`.
- `suite.sh`: every config (`CONFIGS`: selkies with and without picom, voxl and
  KasmVNC with and without picom) x workload
  (Chromium scroll, 1080p and full-screen animation) x link (LAN, 10 Mbps/40 ms,
  5 Mbps/60 ms) via `shaper.py`, a userspace bottleneck proxy.
- `ab.sh <workload> <reps>`: interleaved voxl vs selkies runs.
- `cpusuite.sh`: server CPU (X server + streamer) per workload with one client connected.
- `scroll.html`: the long page `scrolltarget.mjs` smooth-scrolls, with a marker drawn every frame.
- `cpu.sh <pid> <secs>`, `rate.sh <secs>` (voxl `/stats`), `srcfps.py` (true
  source frame rate from the Xvfb framebuffer), `measure.mjs` (in-browser unique FPS).
- `inputtest.mjs`, `keytest.sh`: input and clipboard checks through the web client.

## Feature tests

`tests/` drives the web client with Playwright against a running voxl
(`VOXL_PORT`, default 6903) on a test display (`BENCH_DISPLAY`, default `:3`).
For smooth scrolling, start that display with the patched server:
`XVFB=$(nix-build -E 'with import <nixpkgs> {}; callPackage ../xvfb.nix {}')/bin/Xvfb BENCH_DISPLAY=:3 env/xvfb.sh`.

- `smoothtest.mjs <display> <port>`: precise scroll steps reach Chromium pixel by pixel.
- `featuretest.mjs`: wheel/trackpad scrolling, tap, two-finger scroll, long press,
  touch toolbar (uses `target.html`; `SCROLL_PAGE` for the scroll page).
- `cliptest.mjs`, `cliptext.mjs`: PNG and text clipboard in both directions
  (`TEST_PNG`, needs `xclip`).
- `fstest.mjs`, `fsiframe.mjs`: fullscreen toolbar and Keyboard Lock, top level and embedded.
- `resizetest.mjs`: re-fit in a resizable sidebar iframe and at several tab sizes.
- `filestest.mjs`: file panel, uploads (button and drag and drop) and downloads; starts its own voxl.
- `resizedyn.mjs`: dynamic resolution end to end with its own Xvfb (`XVFB`) and voxl:
  desktop follows the window, clamping, decoded size, pointer mapping, zero-copy capture.
- `mictest.mjs`: microphone end to end with a fake 440 Hz microphone, a private PulseAudio
  pipe source and its own voxl (`PULSEAUDIO`: the pulseaudio package).

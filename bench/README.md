# Benchmarks

Tools used to compare voxl with selkies. Everything runs on a separate Xvfb display
(`:2` by default, `BENCH_DISPLAY` to change it), so a live desktop isn't disturbed.

## Setup

```sh
cc -O2 -o loadgen loadgen.c -lX11 -lXext      # 60 Hz animation with a frame/time marker
cc -O2 -o rootlat rootlat.c -lX11             # marker latency on the root window
env/xvfb.sh & sleep 1; env/openbox.sh &       # 2560x1600 @ 144 DPI, like production
env/voxl.sh                                   # voxl on :6902 (VOXL_BIN to override)
SELKIES_BIN=/path/to/selkies env/selkies.sh & # selkies on :6912
```

`*.mjs` need Playwright (`PLAYWRIGHT_NODE_MODULES`) and Chromium (`CHROMIUM`).
`py.sh` runs Python with PyAV and websockets through nix-shell.

## Measuring

- `nclient.py voxl|selkies host:port secs [x y]`: native client (FFmpeg decode)
  that reads the marker at (x, y). Reports unique FPS, latency and Mbps.
- `suite.sh`: every config (selkies with and without picom, voxl) x workload
  (Chromium scroll, 1080p and full-screen animation) x link (LAN, 10 Mbps/40 ms,
  5 Mbps/60 ms) via `shaper.py`, a userspace bottleneck proxy.
- `ab.sh <workload> <reps>`: interleaved voxl vs selkies runs.
- `cpu.sh <pid> <secs>`, `rate.sh <secs>` (voxl `/stats`), `srcfps.py` (true
  source frame rate from the Xvfb framebuffer), `measure.mjs` (in-browser unique FPS).
- `inputtest.mjs`, `keytest.sh`: input and clipboard checks through the web client.

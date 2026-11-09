# Native Wayland capture prototype

The Linux `--capture-backend=wayland` adapter feeds voxl's existing partial I420
conversion, software x264, paintover, flow control, and browser protocol. The
backend is explicit; `$DISPLAY` is never used to choose or control a Wayland
session. X11 remains the default.

The prototype requires `ext-image-copy-capture-v1`,
`ext-output-image-capture-source-v1`, `wl_output` v4, and XRGB8888/ARGB8888 SHM
buffers. It was exercised with labwc/wlroots 0.20.2 using its headless backend and
Pixman renderer. It does not require a GPU, libwayland at runtime, or a portal.

It is **capture-only and read-only**, with the cursor embedded by the compositor.
Video, screenshots, recording and `/wait` share the existing pipeline. Input,
clipboard, separate cursor metadata, viewer-requested resizing, portal/PipeWire,
and wlr-screencopy fallback remain future work. External output resizing rebuilds
the capture/encoder. Rotated/flipped captures, odd dimensions, and unsupported
formats fail explicitly. Compositor disconnect/output removal currently stops the
server; reconnect recovery is not implemented.

## Buffer and damage handling

Two persistent memfd-backed SHM slots alternate. The compositor writes only the
pending slot; the capture thread reads only a slot that has received `ready`.
Conversion reads directly from its mapping. No intermediate full-frame pixel copy
is added. SHM capture still introduces compositor work that Xvfb's direct
framebuffer access avoids.

The adapter tracks stale macroblocks separately for each capture buffer and sends
their union through `damage_buffer`, allowing the compositor to reduce copying.
A bounded dirty-block map also accumulates source damage until the next encode.
voxl's existing pending row bands independently keep both I420 buffers current.
Dropping a frame under backpressure does not discard its damage.

Capture events are dispatched on the capture thread, using the Wayland socket in
the existing wait set. There is one capture request in flight, with no new capture
worker or raw-frame queue. A pending request on a static screen does not block
snapshots, stop, or other hub wakes. `/wait` observes capture damage without
viewers, while conversion/encoding stay off. On an active screen this still
requires compositor capture work, unlike XDamage-only observation.

## Run an isolated session

Build voxl normally:

```sh
nix-shell --run 'cargo build --release --locked'
```

For a reproducible compositor/tool environment, the sandbox used this package
revision (labwc 0.20.2):

```sh
nix shell github:NixOS/nixpkgs/1d060086d60cb76a40cf37109f979d88a5bd6396#labwc \
  github:NixOS/nixpkgs/1d060086d60cb76a40cf37109f979d88a5bd6396#wlr-randr
```

Inside that shell, use a private runtime directory and an empty config directory,
then keep the compositor running:

```sh
mkdir -p /tmp/voxl-wayland-runtime /tmp/voxl-labwc-config
chmod 700 /tmp/voxl-wayland-runtime
export XDG_RUNTIME_DIR=/tmp/voxl-wayland-runtime
WLR_BACKENDS=headless WLR_HEADLESS_OUTPUTS=1 WLR_RENDERER=pixman \
  labwc -C /tmp/voxl-labwc-config &
# Wait for the socket, then use the actual socket name if it differs.
export WAYLAND_DISPLAY=wayland-0
wlr-randr --output HEADLESS-1 --custom-mode 2560x1600@60Hz
./target/release/voxl --capture-backend=wayland --wayland-output=HEADLESS-1 \
  --addr=127.0.0.1 --port=6920 --audio-enabled=false
```

`--wayland-display` optionally selects a socket name relative to `XDG_RUNTIME_DIR`
or an absolute socket path. Otherwise the Wayland connection uses its standard
environment. Authentication remains enabled by default. Test scripts below expect
an isolated localhost server started with `--enable-basic-auth=false`.

## Reproduce the comparison

Build the native workload with temporary benchmark-only dependencies:

```sh
nix-shell -p pkg-config wayland-scanner wayland wayland-protocols \
  xorg.libX11 xorg.libXext --run bench/wayland/build.sh
```

`loadgen` uses one pixel-rendering implementation with two submission backends:
X11 MIT-SHM and native Wayland `wl_shm`/xdg-shell. It does not render through
XWayland. Both produce the same full-screen texture and timestamp marker.

```sh
bench/wayland/build/loadgen wayland sparse 2560 1600 60 60 demo
# Or DISPLAY=:22 .../loadgen x11 scroll 2560 1600 60 60 demo
```

Modes are `scroll` (full-frame moving document), `sparse` (480×48 marker updates),
`idle` (one frame), and `manual` (SIGUSR1 advances changes in two distant patches).
Both sessions must be 2560×1600 for the comparison. Use an isolated Xvfb display
with `-shmem`; do not repurpose a user's active desktop.

Start a voxl instance per backend with identical settings: 60 fps, CRF 24,
`--encoder-threads=1 --convert-threads=4 --min-kbps=30000 --max-kbps=30000`,
no audio, one stripe, read-only, and authentication disabled on localhost. The
30 Mbps setting is the common adaptive-rate ceiling/floor, not fixed-rate CBR.
Check the X11 startup log confirms direct framebuffer access. Run the compositor
without debug logging. Set `DISPLAY`, `WAYLAND_DISPLAY`, and `XDG_RUNTIME_DIR` for
the two source sessions, then:

```sh
bench/py.sh bench/wayland/compare.py \
  6920 WAYLAND_VOXL_PID LABWC_PID 6921 X11_VOXL_PID XVFB_PID
```

The runner performs two passes, reverses backend order on the second, and writes
JSON samples into ignored `bench/wayland/results/`. Each sample includes a 4-second
no-viewer interval, 4 seconds of viewer warmup, and 12 seconds of measurement. The
native PyAV client ACKs after decoding and reads the source's timestamp marker.
These are draw-to-decoded measurements on localhost, not browser input-to-photon
latency. CPU is reported per process in percent of one CPU; the server+compositor
sum excludes the workload and decoder, with workload CPU separately recorded.

## Behavior checks

Start a **fresh** manual workload per check, then run:

```sh
bench/py.sh bench/wayland/smoke.py 127.0.0.1:6920 LOADGEN_PID wayland
```

The same check runs against X11 by changing the endpoint and final argument. It
checks real screenshot pixels and sequence numbers, regional `/wait`, activity
without encoding when no viewers are connected, alternating buffers, decoded
H.264 pixels, damage retention after deliberate ACK backpressure, and cessation
of encoding after the screen settles. It assumes no other viewers/workloads.

The standard Rust tests also exercise bounded damage accumulation and edge
clipping. `cargo package --locked --allow-dirty` checks that the new source files
are included in the distributable crate. See [the measured results](results.md)
for the sandbox observations and limits.

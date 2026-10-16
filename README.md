# voxl

Low-latency desktop streaming for the browser, written in Rust. voxl captures an X11
display, encodes it as H.264 and streams it over a WebSocket to a small web client
that decodes with WebCodecs. It powers the Computer preview in Tembo and replaces
[selkies](https://github.com/selkies-project/selkies) there; it accepts selkies'
command-line flags, so it drops into the same service definition.

## Why

Measured on a Tembo VM (16 vCPU AMD EPYC 9334, no GPU, 2560x1600 Xvfb desktop), same
apps, same decoding client, against selkies as deployed (picom on):

| Test | selkies: FPS (worst second), latency p50/p99, Mbps | voxl |
|---|---|---|
| Chromium scroll, LAN | 56.8 (54.0), 85/106 ms, 8.6 | **60.1 (59.7), 55/60 ms, 3.6** |
| Scroll, 10 Mbps / 40 ms RTT | 55.6 (47.7), 152/261 ms, 8.7 | **60.0 (59.4), 82/94 ms, 3.6** |
| Scroll, 5 Mbps / 60 ms RTT | 27.9 (19.6), 2.7 s / 3.7 s | **60.1 (59.7), 99/109 ms** |
| 1080p animation, LAN | 58.7 (55.9), 31/48 ms, 13.0 | **60.0 (59.7), 14/18 ms, 7.3** |
| Animation, 10 Mbps / 40 ms RTT | 39.3 (31.8), 2.3 s / 3.1 s | **60.1 (59.5), 47/80 ms** |
| Animation, 5 Mbps / 60 ms RTT | 18.4 (15.3), 2.2 s / 4.3 s | **59.9 (57.4), 66/93 ms** |
| Full-screen animation, LAN | 54.8 (49.8), 54/79 ms, 13.4 | **59.2–60.1, 19–21/25–27 ms, 5.2** |

| Server CPU (cores) | idle | small update | scroll | full screen |
|---|---|---|---|---|
| selkies | 1.29 | 1.38 | 1.54 | 1.80 |
| voxl | **0.00** | **0.98** | **1.33** | **1.68** |

FPS counts unique frames displayed (read from a pixel marker drawn into the content),
and latency runs from the moment the content was drawn to the decoded frame. See
[`bench/`](bench/README.md) to reproduce.

## How it works

- **Capture:** voxl attaches Xvfb's `-shmem` framebuffer directly (zero copy, no X
  requests per frame) and falls back to MIT-SHM `GetImage` on other servers.
  XDamage raw rectangles say what changed, so nothing is captured while the screen
  is still and changed regions arrive without a server round trip.
- **Convert:** only the damaged rows are converted to I420 (libyuv, multi-threaded).
- **Encode:** x264 (`ultrafast`, `zerolatency`, 4 sliced threads) with no lookahead,
  no B-frames, no VFR delay and no periodic keyframes. Per-macroblock "unchanged"
  hints from damage let x264 skip static areas. When the screen goes still, a short
  paint-over burst at higher quality sharpens text.
- **Flow control:** clients ack each frame once it is decoded. Frames are dropped
  *before* encoding when acks fall behind, so latency stays bounded instead of
  queueing.
- **Adaptive bitrate:** delay-based control. It starts at 8 Mbps, ramps quickly
  while the path is clear, remembers the throughput where queueing began, and probes
  past it gently, so quality drops before frame rate does.
- **Client:** decode and the socket run in a Web Worker. Frames go to the
  compositor through `MediaStreamTrackGenerator` and a `<video>` (Chromium), or are
  drawn to an `OffscreenCanvas` elsewhere, as soon as they are decoded. The cursor is
  drawn locally from XFixes cursor images (the X cursor is hidden), so pointer
  movement has no stream latency.
- **Rounded corners without a compositor:** `--round-corners` gives top-level
  windows a rounded X SHAPE when they map or resize, so the X server shows the
  desktop through the corners. Unlike picom on Xvfb, nothing is repainted per frame.
- **Input:** XTEST injection by keysym, with spare keycodes that browsers don't map
  remapped for characters missing from the keymap. Scrolling is pixel-precise on an
  X server built with [`patches/xorg-server-xtest-scroll.patch`](patches/) (see
  `xvfb.nix`), which gives the XTEST pointer smooth-scroll axes; elsewhere it falls
  back to wheel clicks.
- **Touch:** tap to click, drag, long press for right click, two-finger scroll, and
  an on-screen keyboard button on touch devices.
- **Fullscreen:** a toolbar button (top-right on hover) enters fullscreen and takes
  a Keyboard Lock so browser shortcuts reach the desktop. Browsers only allow the
  lock from the top-level page, so an embedding page should call
  `navigator.keyboard.lock()` when the voxl iframe becomes the fullscreen element.
- **Clipboard:** CLIPBOARD sync both ways for text and PNG images, including INCR
  transfers and paste ordering.
- **Audio:** PulseAudio monitor audio is sent as Opus.

## Build

Needs Rust, pkg-config, clang (bindgen), x264, libyuv, libopus and libpulse.

```sh
nix-shell          # or provide the libraries yourself
cargo build --release
```

Smooth scrolling needs the patched Xvfb:

```sh
nix-build -E 'with import <nixpkgs> {}; callPackage ./xvfb.nix {}'
```

## Run

```sh
DISPLAY=:1 ./target/release/voxl --port=6901
```

Open `http://host:6901/` (add `?stats` for an FPS/bitrate overlay). Start Xvfb with
`-shmem` for direct framebuffer capture. Turn off compositors that redraw the whole
screen on every change (picom's xrender backend on Xvfb caps apps at ~40 FPS and
adds ~15 ms).

| Flag | Default | |
|---|---|---|
| `--addr`, `--port` | `0.0.0.0`, `6901` | listen address |
| `--framerate` | `60` | max frames per second (`60-60` style ranges accepted) |
| `--preset` | `ultrafast` | x264 preset |
| `--encoder-threads` | `min(4, ncpu/4)` | x264 sliced threads |
| `--crf` | `24` | base quality |
| `--max-kbps` / `--video-bitrate` | `30000` | adaptive bitrate ceiling |
| `--min-kbps` | `2000` | adaptive bitrate floor |
| `--paintover-crf`, `--paintover-burst`, `--paintover-delay-ms` | `16`, `3`, `150` | sharpening when the screen goes still |
| `--enable-clipboard` | `true` | `true`, `in`, `out` or `false` |
| `--audio-enabled`, `--audio-device-name`, `--audio-bitrate` | `true`, `output.monitor`, `128000` | desktop audio |
| `--round-corners` | `0` (off) | corner radius in pixels for top-level windows (Tembo uses `12`) |
| `--round-windows` | none | `title:radius,...` rounds windows with that title, docks included |
| `--adaptive-threads` | `false` | encode on one thread, fanning out only under sustained heavy motion |
| `--stripes` | `1` | encode the screen as N independent horizontal streams (experimental) |
| `--scroll-px-per-notch` | `180` | desktop pixels one wheel notch scrolls (calibrates smooth scrolling) |
| `--convert-threads`, `--coalesce-us` | `4`, `0` | tuning |

Other selkies flags are accepted and ignored. `GET /stats` returns server counters,
and `GET /healthz` returns `ok`.

Debug environment variables: `VOXL_NO_DIRECT_FB`, `VOXL_NO_MBINFO`, `VOXL_X264_INFO`,
`VOXL_DEBUG_DAMAGE`, `VOXL_DEBUG_CORNERS`, `VOXL_DEBUG_STRIPES`, `VOXL_DEBUG_MARKER=x,y`.

## Protocol

One WebSocket at `/ws`. Binary server messages start with a type byte: `0x01` video
(16-byte header with flags, frame id, server pipeline time, stripe index/count, parts
in frame and stripe y, followed by Annex-B H.264), `0x02` Opus audio, `0x03` cursor
image (RGBA), `0x04` clipboard image (PNG). Text messages are JSON (`hello`, `clip`, `ping`). Client messages are
short comma-separated commands: `a,<frame>` ack, `m,x,y` move, `b,btn,0|1`,
`s,dx,dy` wheel clicks, `sp,dx,dy` scroll in desktop pixels, `k,keysym,0|1`,
`c,<text>` clipboard text, binary `0x11`+PNG clipboard image, `v,0|1` video on/off,
`au,0|1` audio, `kf` keyframe request, `po,<t>` pong.

## Limitations

X11 only, single display, fixed resolution, H.264 4:2:0, desktop audio out only (no
microphone), no file transfer.

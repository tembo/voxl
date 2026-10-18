# voxl

Low-latency desktop streaming for the browser, written in Rust. voxl captures an X11
display, encodes it as H.264 and streams it over a WebSocket to a small web client
that decodes with WebCodecs. It powers the Computer preview in Tembo and replaces
[selkies](https://github.com/selkies-project/selkies) there; it accepts selkies'
command-line flags, so it drops into the same service definition.

## Why

Measured on a Tembo VM (16 vCPU AMD EPYC 9334, no GPU, 2560x1600 Xvfb desktop), same
apps, same decoding client, against selkies as deployed (picom on):

| Test                           | selkies: FPS (worst second), latency p50/p99, Mbps | voxl                               |
| ------------------------------ | -------------------------------------------------- | ---------------------------------- |
| Chromium scroll, LAN           | 56.8 (54.0), 85/106 ms, 8.6                        | **60.1 (59.7), 55/60 ms, 3.6**     |
| Scroll, 10 Mbps / 40 ms RTT    | 55.6 (47.7), 152/261 ms, 8.7                       | **60.0 (59.4), 82/94 ms, 3.6**     |
| Scroll, 5 Mbps / 60 ms RTT     | 27.9 (19.6), 2.7 s / 3.7 s                         | **60.1 (59.7), 99/109 ms**         |
| 1080p animation, LAN           | 58.7 (55.9), 31/48 ms, 13.0                        | **60.0 (59.7), 14/18 ms, 7.3**     |
| Animation, 10 Mbps / 40 ms RTT | 39.3 (31.8), 2.3 s / 3.1 s                         | **60.1 (59.5), 47/80 ms**          |
| Animation, 5 Mbps / 60 ms RTT  | 18.4 (15.3), 2.2 s / 4.3 s                         | **59.9 (57.4), 66/93 ms**          |
| Full-screen animation, LAN     | 54.8 (49.8), 54/79 ms, 13.4                        | **59.2–60.1, 19–21/25–27 ms, 5.2** |

| Server CPU (cores) | idle     | small update | scroll   | full screen |
| ------------------ | -------- | ------------ | -------- | ----------- |
| selkies            | 1.29     | 1.38         | 1.54     | 1.80        |
| voxl               | **0.00** | **0.98**     | **1.33** | **1.68**    |

FPS counts unique frames displayed (read from a pixel marker drawn into the content),
and latency runs from the moment the content was drawn to the decoded frame. See
[`bench/`](bench/README.md) to reproduce.

### Against KasmVNC

Same VM and workloads, against KasmVNC 1.4.0 with the settings of Tembo's `kasmvnc`
backend (60 FPS cap, JPEG/WebP quality 8–9, video mode). KasmVNC is its own X server,
so it runs the apps on its own display; both are measured without a compositor, with
a native client that reads the marker like the one above.

| Test                           | KasmVNC: FPS (worst second), latency p50/p99, Mbps | voxl                           |
| ------------------------------ | -------------------------------------------------- | ------------------------------ |
| Chromium scroll, LAN           | 51.7 (39.5), 99/135 ms, 32.7                       | **60.1 (59.7), 72/81 ms, 3.6** |
| Scroll, 10 Mbps / 40 ms RTT    | 8.3 (7.4), 229/249 ms                              | **60.0 (59.1), 98/107 ms**     |
| Scroll, 5 Mbps / 60 ms RTT     | 4.9 (3.6), 306/415 ms                              | **60.1 (59.5), 129/154 ms**    |
| 1080p animation, LAN           | 56.7 (51.6), 51/82 ms, 2.0                         | **60.1 (59.9), 12/16 ms**, 7.3 |
| Animation, 10 Mbps / 40 ms RTT | 52.9 (49.2), 76/115 ms                             | **60.1 (59.5), 46/71 ms**      |
| Animation, 5 Mbps / 60 ms RTT  | 34.1 (26.4), 104/156 ms                            | **60.0 (58.8), 66/92 ms**      |
| Full-screen animation, LAN     | 34.2 (32.5), 83/102 ms, 1.9                        | **59.8 (54.0), 18/25 ms**, 5.1 |

KasmVNC spots the animation's scrolling and sends it as copies of pixels the client
already has, which is why it needs less bandwidth there. Its own web client shows the
same picture (42.7 FPS for the LAN scroll, 8 FPS at 10 Mbps). With picom running, as in
the Computer preview, KasmVNC falls to 25–36 FPS on LAN, while voxl holds 60 (51 for
the full-screen animation).

| Server CPU, X server + streamer (cores) | idle | 1080p animation | scroll   | full screen      |
| --------------------------------------- | ---- | --------------- | -------- | ---------------- |
| KasmVNC                                 | 0.00 | 1.97            | 2.07     | 1.82 (at 34 FPS) |
| voxl                                    | 0.00 | **1.39**        | **1.38** | 1.83 (at 60 FPS) |

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
  _before_ encoding when acks fall behind, so latency stays bounded instead of
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
- **Dynamic resolution:** with `--enable-resize`, the desktop follows the viewer's
  window instead of being scaled into it: the client asks for its window size times
  `--resize-scale` (default 1.5, matching a 144 DPI desktop, so windows look as big
  as in the viewer) on connect and, debounced, whenever the window changes. voxl
  resizes the X screen with RandR (width rounded to 8, height to 2, clamped between
  640x480 and the size Xvfb started with), rebuilds capture and encode at the new
  size and sends clients the new geometry with a keyframe. The latest request wins.
  Start Xvfb at the largest size to allow; it keeps the same `-shmem` framebuffer
  across sizes, so capture stays zero-copy. `?scale=` on the page overrides the scale.
  Apps that position things in screen pixels (and agents taking screenshots) see the
  size change.
- **Microphone:** with `--microphone-enabled`, a mic button in the toolbar captures the
  browser's microphone (echo cancellation and noise suppression on), encodes it as
  Opus with WebCodecs (16-bit PCM where that's missing) and sends it on the same
  socket. voxl writes it into a FIFO that PulseAudio reads as the VM's microphone:
  ```
  load-module module-pipe-source source_name=microphone file=/tmp/voxl-mic.fifo format=s16le rate=48000 channels=1
  set-default-source microphone
  ```
  voxl paces the FIFO with its own 20 ms clock and writes silence while the mic is
  off, so apps recording it never stall, and a 40–120 ms jitter queue keeps delay
  bounded. One viewer holds the mic at a time; turning it on elsewhere takes it over.
  The toolbar stays visible while the mic is live. An embedding page must allow
  `microphone` on the iframe.
- **Files:** a Files button (top-right on hover) opens a panel to browse the
  desktop user's home folder and download files, newest first. Files dropped
  anywhere on the screen, or picked with Upload, go to the open folder
  (`~/Downloads` by default). Transfers use their own HTTP requests, so they never
  hold up the stream. Uploads are written to a hidden temporary file and renamed
  when complete, and existing names get a ` (1)` suffix instead of being replaced.
- **Look:** the toolbar and Files panel use the Tembo app's UI tokens, icons and
  Inter (`web/inter.woff2`, a Latin subset under the SIL Open Font License, see
  `web/inter-LICENSE.txt`).

## Build

Needs Rust, pkg-config, clang (bindgen), x264, libyuv, libopus and libpulse, plus
esbuild and TypeScript for the web client.

```sh
nix-shell          # or provide the tools and libraries yourself
cargo build --release
```

The web client is TypeScript in `web/src`, with no npm dependencies. `build.rs`
bundles it with esbuild (`ESBUILD` to point at a specific binary) and the binary
embeds the result, along with the static files in `web/` (`index.html`, the font
and the icon). `protocol.ts` defines the wire format and the page/worker messages.
The page, the worker and the audio worklets run in different browser environments,
so each has its own type-check config (the Nix build runs all three):

```sh
tsc -p web && tsc -p web/tsconfig.worker.json && tsc -p web/tsconfig.worklet.json
```

Smooth scrolling needs the patched Xvfb:

```sh
nix-build -E 'with import <nixpkgs> {}; callPackage ./xvfb.nix {}'
```

### Formatting and linting

CI runs these on every pull request (`.github/workflows/ci.yml`):

```sh
npm ci && npm run check      # oxfmt --check, oxlint and tsc for the web client
nix-shell --arg withRust true --run 'cargo fmt --check && cargo clippy --all-targets --locked -- -D warnings'
```

`npm run fmt` and `cargo fmt` fix formatting. The npm packages are dev tools only;
the build doesn't use them.

## Run

```sh
DISPLAY=:1 ./target/release/voxl --port=6901
```

Open `http://host:6901/` (add `?stats` for an FPS/bitrate overlay). Start Xvfb with
`-shmem` for direct framebuffer capture. Turn off compositors that redraw the whole
screen on every change (picom's xrender backend on Xvfb caps apps at ~40 FPS and
adds ~15 ms).

| Flag                                                           | Default                            |                                                                                   |
| -------------------------------------------------------------- | ---------------------------------- | --------------------------------------------------------------------------------- |
| `--addr`, `--port`                                             | `0.0.0.0`, `6901`                  | listen address                                                                    |
| `--framerate`                                                  | `60`                               | max frames per second (`60-60` style ranges accepted)                             |
| `--preset`                                                     | `ultrafast`                        | x264 preset                                                                       |
| `--encoder-threads`                                            | `min(4, ncpu/4)`                   | x264 sliced threads                                                               |
| `--crf`                                                        | `24`                               | base quality                                                                      |
| `--max-kbps` / `--video-bitrate`                               | `30000`                            | adaptive bitrate ceiling                                                          |
| `--min-kbps`                                                   | `2000`                             | adaptive bitrate floor                                                            |
| `--paintover-crf`, `--paintover-burst`, `--paintover-delay-ms` | `16`, `3`, `150`                   | sharpening when the screen goes still                                             |
| `--enable-clipboard`                                           | `true`                             | `true`, `in`, `out` or `false`                                                    |
| `--audio-enabled`, `--audio-device-name`, `--audio-bitrate`    | `true`, `output.monitor`, `128000` | desktop audio                                                                     |
| `--enable-resize`, `--resize-scale`                            | `false`, `1.5`                     | desktop follows the viewer's window; desktop pixels per CSS pixel                 |
| `--microphone-enabled`, `--microphone-fifo`                    | `false`, `/tmp/voxl-mic.fifo`      | browser microphone into the FIFO a PulseAudio pipe source reads                   |
| `--round-corners`                                              | `0` (off)                          | corner radius in pixels for top-level windows (Tembo uses `12`)                   |
| `--round-windows`                                              | none                               | `title:radius,...` rounds windows with that title, docks included                 |
| `--adaptive-threads`                                           | `false`                            | encode on one thread, fanning out only under sustained heavy motion               |
| `--stripes`                                                    | `1`                                | encode the screen as N independent horizontal streams (experimental)              |
| `--scroll-px-per-notch`                                        | `180`                              | desktop pixels one wheel notch scrolls (calibrates smooth scrolling)              |
| `--file-transfers`                                             | `upload,download`                  | `upload`, `download`, both, or `none`                                             |
| `--files-root`                                                 | `$HOME`                            | folder the Files panel can see; nothing outside it is reachable                   |
| `--upload-dir`                                                 | `Downloads`                        | folder (under the root) the panel opens and uploads into; created on first upload |
| `--max-upload-mb`                                              | `0` (no limit)                     | largest accepted upload                                                           |
| `--convert-threads`, `--coalesce-us`                           | `4`, `0`                           | tuning                                                                            |

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
`au,0|1` audio, `sz,w,h` desktop size request (dynamic resolution), `mi,0|1` microphone off/on, binary `0x12`+Opus or `0x13`+PCM (s16le
mono 48 kHz) microphone audio, `kf` keyframe request, `po,<t>` pong.

Files are plain HTTP: `GET /files/list?path=<dir>` returns JSON entries (`name`, `dir`, `size`, `mtime` in ms),
`GET /files/download?path=<file>` streams a file as an attachment, and
`PUT /files/upload?path=<dir>&name=<file>` takes the raw file as the body. Uploads
must send an `X-Voxl-Upload` header; browsers only send custom headers cross-origin
after a CORS preflight, which voxl never answers, so other sites can't upload.
Paths are relative to `--files-root`; `..`, absolute paths and symlinks leading
outside it are refused.

## Limitations

X11 only, single display, H.264 4:2:0, resizing only up to the size Xvfb started with, single files only (no folder
upload or download).

# voxl

Low-latency desktop streaming for the browser, written in Rust. voxl streams an X11
display, a Mac's screen or a Windows desktop as H.264 over a WebSocket to a small web client that decodes with WebCodecs,
with input, clipboard, audio, microphone, file and folder transfer and dynamic resolution.

## Benchmarks

A 16 vCPU AMD EPYC 9334 VM (no GPU), 2560x1600 Xvfb desktop, same workloads, each
stream decoded by a native client that reads a pixel marker drawn into the content.
Cells are FPS (worst second), then latency p50/p99 from the content being drawn to the
frame being decoded. voxl's column is its best run of each test. To reproduce, see
[`bench/`](bench/README.md).

| Test                           | voxl                       | [selkies](https://github.com/selkies-project/selkies) | [KasmVNC](https://github.com/kasmtech/KasmVNC) | [Neko](https://github.com/m1k1o/neko) | [Neko, tuned](https://github.com/m1k1o/neko) | [noVNC](https://github.com/novnc/noVNC) | [Daytona](https://www.daytona.io) |
| ------------------------------ | -------------------------- | ----------------------------------------------------- | ---------------------------------------------- | ------------------------------------- | -------------------------------------------- | --------------------------------------- | --------------------------------- |
| Chromium scroll, LAN           | **60.1 (59.7), 55/60 ms**  | 56.8 (54.0), 85/106 ms                                | 51.7 (39.5), 99/135 ms                         | 25.0 (23.7), 156/217 ms               | 53.9 (49.9), 128/155 ms                      | 6.7 (6.3), 198/301 ms                   | 5.2 (2.5), 306/713 ms             |
| Scroll, 10 Mbps / 40 ms RTT    | **60.0 (59.4), 82/94 ms**  | 55.6 (47.7), 152/261 ms                               | 8.3 (7.4), 229/249 ms                          | 24.6 (14.9), 221/737 ms               | 46.3 (40.3), 606/831 ms                      | 2.6 (2.4), 426/678 ms                   | 1.9 (1.6), 617/798 ms             |
| Scroll, 5 Mbps / 60 ms RTT     | **60.1 (59.7), 99/109 ms** | 27.9 (19.6), 2.7/3.7 s                                | 4.9 (3.6), 306/415 ms                          | 21.7 (3.8), 1.2/3.6 s                 | stalls                                       | 1.6 (1.4), 637/937 ms                   | 1.1 (0.9), 1.1/1.2 s              |
| 1080p animation, LAN           | **60.1 (59.9), 12/16 ms**  | 58.7 (55.9), 31/48 ms                                 | 56.7 (51.6), 51/82 ms                          | 24.8 (23.6), 122/202 ms               | 55.6 (50.0), 54/82 ms                        | 7.7 (7.2), 149/210 ms                   | 10.5 (8.8), 138/209 ms            |
| Animation, 10 Mbps / 40 ms RTT | **60.1 (59.5), 46/71 ms**  | 39.3 (31.8), 2.3/3.1 s                                | 52.9 (49.2), 76/115 ms                         | 25.1 (9.1), 295/962 ms                | 33.3 (25.4), 4.0/7.1 s                       | 5.8 (5.2), 160/226 ms                   | 7.4 (7.0), 154/213 ms             |
| Animation, 5 Mbps / 60 ms RTT  | **60.0 (58.8), 66/92 ms**  | 18.4 (15.3), 2.2/4.3 s                                | 34.1 (26.4), 104/156 ms                        | 15.2 (8.6), 5.0/9.4 s                 | stalls                                       | 5.1 (4.7), 174/240 ms                   | 5.8 (4.4), 189/252 ms             |
| Full-screen animation, LAN     | **59.8 (54.0), 18/25 ms**  | 54.8 (49.8), 54/79 ms                                 | 34.2 (32.5), 83/102 ms                         | 22.7 (18.0), 2.9/3.3 s                | 54.6 (49.7), 54/79 ms                        | 5.9 (5.2), 184/279 ms                   | 9.1 (7.2), 154/247 ms             |

| Server CPU, X server + streamer (cores)               | idle | 1080p animation  | scroll           | full screen      |
| ----------------------------------------------------- | ---- | ---------------- | ---------------- | ---------------- |
| voxl                                                  | 0.00 | 1.39             | 1.33             | 1.68             |
| [selkies](https://github.com/selkies-project/selkies) | 1.29 | not measured     | 1.54             | 1.80             |
| [KasmVNC](https://github.com/kasmtech/KasmVNC)        | 0.00 | 1.97             | 2.07             | 1.82 (at 34 FPS) |
| [Neko](https://github.com/m1k1o/neko)                 | 1.63 | 2.50 (at 25 FPS) | 2.20 (at 25 FPS) | 2.46 (at 23 FPS) |
| [Neko](https://github.com/m1k1o/neko), tuned          | 1.90 | 2.28             | 2.30             | 2.52             |
| [noVNC](https://github.com/novnc/noVNC)               | 0.01 | 0.55 (at 8 FPS)  | 0.58 (at 7 FPS)  | 0.64 (at 6 FPS)  |
| [Daytona](https://www.daytona.io) (1 CPU limit)       | 0.02 | 0.82 (at 10 FPS) | 0.53 (at 5 FPS)  | 0.81 (at 9 FPS)  |

- **selkies:** [selkies](https://github.com/selkies-project/selkies) with picom running.
- **KasmVNC:** 1.4.0 with a 60 FPS cap, JPEG/WebP quality 8–9 and video mode, no
  compositor.
- **Neko:** [m1k1o/neko](https://github.com/m1k1o/neko) 3.2 with its defaults (VP8, 25
  FPS). **Neko, tuned** captures at 60 FPS and encodes with x264 `ultrafast`/
  `zerolatency` at up to 30 Mbps, like voxl. Neither adapts its bitrate (Neko's
  estimator is off by default), so on slow links frames queue up; "stalls" means no
  frame arrived in 20 s.
- **noVNC:** x11vnc and noVNC as [E2B Desktop](https://github.com/e2b-dev/desktop)
  and Daytona ship them (`-forever -wait 50 -shared`, Tight encoding). x11vnc polls the
  screen, which caps it at 6–8 FPS.
- **Daytona:** a default sandbox (1 CPU, x11vnc and noVNC at 2560x1600) streamed
  through Daytona's signed preview proxy, about 12 ms away, which its latency includes.
  The workloads run inside the sandbox, sharing its CPU, and slow links are added on top
  of that internet path.
- **Links:** a bottleneck with a deep queue: a TCP proxy for the WebSocket servers, and
  `tc netem` for Neko's WebRTC media (UDP).

## Usage

Build (needs Rust, pkg-config, clang, x264, libyuv, libopus, libpulse and esbuild;
`nix-shell` provides them all):

```sh
nix-shell
cargo build --release
```

Run it against an X display, then open `http://host:6901/` (`?stats` adds an FPS
overlay):

```sh
DISPLAY=:1 ./target/release/voxl --port=6901
```

Start Xvfb with `-shmem` for zero-copy capture, and use the patched Xvfb
(`nix-build -E 'with import <nixpkgs> {}; callPackage ./xvfb.nix {}'`) for smooth
scrolling. voxl accepts selkies' command-line flags; other selkies flags are ignored.

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
| `--round-corners`                                              | `0` (off)                          | corner radius in pixels for top-level windows                                     |
| `--round-windows`                                              | none                               | `title:radius,...` rounds windows with that title, docks included                 |
| `--adaptive-threads`                                           | `false`                            | encode on one thread, fanning out only under sustained heavy motion               |
| `--stripes`                                                    | `1`                                | encode the screen as N independent horizontal streams (experimental)              |
| `--scroll-px-per-notch`                                        | `180`                              | desktop pixels one wheel notch scrolls (calibrates smooth scrolling)              |
| `--file-transfers`                                             | `upload,download`                  | `upload`, `download`, both, or `none`                                             |
| `--files-root`                                                 | `$HOME`                            | folder the Files panel can see; nothing outside it is reachable                   |
| `--upload-dir`                                                 | `Downloads`                        | folder (under the root) the panel opens and uploads into; created on first upload |
| `--max-upload-mb`                                              | `0` (no limit)                     | largest accepted upload                                                           |
| `--max-record-seconds`                                         | `300`                              | longest recording (`/record`)                                                     |
| `--convert-threads`, `--coalesce-us`                           | `4`, `0`                           | tuning                                                                            |

Screenshots and recordings, for agents and scripts:

```sh
curl -o shot.png localhost:6901/screenshot                  # the screen as a PNG
curl -o clip.mp4 "localhost:6901/record?seconds=10"         # an MP4 of the next 10 s
id=$(curl -s -X POST localhost:6901/record/start | jq .id)  # or start...
curl -o clip.mp4 -X POST "localhost:6901/record/stop?id=$id" # ...and stop when done
```

A recording is the H.264 voxl already sends to viewers, saved as MP4 without encoding
it again, so it costs no extra CPU. It doesn't include the cursor (viewers draw it), and
a resize ends it.

### macOS

voxl also streams a Mac's own screen, with every feature of the Linux version
(ScreenCaptureKit capture, CGEvent input, NSPasteboard clipboard, system audio).

```sh
brew install x264 opus pkgconf esbuild
cargo build --release
./target/release/voxl --port=6901
```

Open `http://localhost:6901/`. On a Mac voxl listens on `127.0.0.1` unless `--addr`
says otherwise, since the stream is your whole screen and there is no login. macOS asks
voxl (or the terminal that starts it) for Screen Recording and Accessibility
permissions. For the microphone, install a loopback device (`brew install
blackhole-2ch`) and pick it as the input in the apps that should hear you.

| macOS flag             | Default                     |                                                                                        |
| ---------------------- | --------------------------- | -------------------------------------------------------------------------------------- |
| `--display`            | main display                | display to stream: an index (0 is the main display) or a display ID; logged at startup |
| `--capture-scale`      | native, within the budget   | streamed pixels per point                                                              |
| `--max-capture-pixels` | `4096000` (2560x1600)       | pixel budget for the automatic scale: x264's cost grows with area                      |
| `--microphone-device`  | first loopback device found | output device (name or prefix) to play the browser microphone into                     |
| `--resize-scale`       | the capture scale           | so a viewer's window maps to the same number of points on the Mac                      |

The other flags work as on Linux; `--audio-device-name`, `--microphone-fifo` and the
rounded-corner flags don't apply.

### Windows

voxl also streams a Windows desktop (Windows 10 or later), with every feature of the
Linux version (DXGI Desktop Duplication capture, SendInput, clipboard text and images,
WASAPI system audio). Build it with Rust, [vcpkg](https://vcpkg.io), LLVM (for bindgen)
and esbuild:

```powershell
vcpkg install x264 libyuv opus --triplet x64-windows-static-md
$env:VCPKGRS_TRIPLET = 'x64-windows-static-md'
npm install -g esbuild
cargo build --release
.\target\release\voxl.exe --port=6901
```

Open `http://localhost:6901/`. As on a Mac, voxl listens on `127.0.0.1` unless `--addr`
says otherwise. It streams the desktop of the session it runs in, so start it in the
user's session (not as a service); Windows hides the lock screen and UAC prompts from
it unless it runs as SYSTEM. For the microphone, install a virtual audio cable
([VB-CABLE](https://vb-audio.com/Cable/)): voxl plays into "CABLE Input" and apps
record from "CABLE Output".

| Windows flag          | Default                   |                                                                    |
| --------------------- | ------------------------- | ------------------------------------------------------------------ |
| `--display`           | primary display           | display to stream, by index (0 is the primary); logged at startup  |
| `--audio-device-name` | default output device     | output device (name or prefix) whose sound to stream               |
| `--microphone-device` | first virtual cable found | output device (name or prefix) to play the browser microphone into |
| `--resize-scale`      | the display's scaling     | so a viewer's window maps to the same size on the desktop          |

The other flags work as on Linux; `--microphone-fifo` and the rounded-corner flags don't
apply. Dynamic resolution picks the largest display mode that fits the viewer's window,
and Windows restores the original mode when voxl exits.

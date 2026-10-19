# voxl

Low-latency desktop streaming for the browser, written in Rust. voxl streams an X11
display as H.264 over a WebSocket to a small web client that decodes with WebCodecs,
with input, clipboard, audio, microphone, file and folder transfer and dynamic resolution.

## Benchmarks

A 16 vCPU AMD EPYC 9334 VM (no GPU), 2560x1600 Xvfb desktop, same workloads, each
stream decoded by a native client that reads a pixel marker drawn into the content.
Cells are FPS (worst second), then latency p50/p99 from the content being drawn to the
frame being decoded. voxl's column is its best run of each test. To reproduce, see
[`bench/`](bench/README.md).

| Test                           | voxl                       | selkies                 | KasmVNC                 | Neko                    | Neko, tuned             | noVNC                 |
| ------------------------------ | -------------------------- | ----------------------- | ----------------------- | ----------------------- | ----------------------- | --------------------- |
| Chromium scroll, LAN           | **60.1 (59.7), 55/60 ms**  | 56.8 (54.0), 85/106 ms  | 51.7 (39.5), 99/135 ms  | 25.0 (23.7), 156/217 ms | 53.9 (49.9), 128/155 ms | 6.7 (6.3), 198/301 ms |
| Scroll, 10 Mbps / 40 ms RTT    | **60.0 (59.4), 82/94 ms**  | 55.6 (47.7), 152/261 ms | 8.3 (7.4), 229/249 ms   | 24.6 (14.9), 221/737 ms | 46.3 (40.3), 606/831 ms | 2.6 (2.4), 426/678 ms |
| Scroll, 5 Mbps / 60 ms RTT     | **60.1 (59.7), 99/109 ms** | 27.9 (19.6), 2.7/3.7 s  | 4.9 (3.6), 306/415 ms   | 21.7 (3.8), 1.2/3.6 s   | stalls                  | 1.6 (1.4), 637/937 ms |
| 1080p animation, LAN           | **60.1 (59.9), 12/16 ms**  | 58.7 (55.9), 31/48 ms   | 56.7 (51.6), 51/82 ms   | 24.8 (23.6), 122/202 ms | 55.6 (50.0), 54/82 ms   | 7.7 (7.2), 149/210 ms |
| Animation, 10 Mbps / 40 ms RTT | **60.1 (59.5), 46/71 ms**  | 39.3 (31.8), 2.3/3.1 s  | 52.9 (49.2), 76/115 ms  | 25.1 (9.1), 295/962 ms  | 33.3 (25.4), 4.0/7.1 s  | 5.8 (5.2), 160/226 ms |
| Animation, 5 Mbps / 60 ms RTT  | **60.0 (58.8), 66/92 ms**  | 18.4 (15.3), 2.2/4.3 s  | 34.1 (26.4), 104/156 ms | 15.2 (8.6), 5.0/9.4 s   | stalls                  | 5.1 (4.7), 174/240 ms |
| Full-screen animation, LAN     | **59.8 (54.0), 18/25 ms**  | 54.8 (49.8), 54/79 ms   | 34.2 (32.5), 83/102 ms  | 22.7 (18.0), 2.9/3.3 s  | 54.6 (49.7), 54/79 ms   | 5.9 (5.2), 184/279 ms |

| Server CPU, X server + streamer (cores) | idle | 1080p animation  | scroll           | full screen      |
| --------------------------------------- | ---- | ---------------- | ---------------- | ---------------- |
| voxl                                    | 0.00 | 1.39             | 1.33             | 1.68             |
| selkies                                 | 1.29 | not measured     | 1.54             | 1.80             |
| KasmVNC                                 | 0.00 | 1.97             | 2.07             | 1.82 (at 34 FPS) |
| Neko                                    | 1.63 | 2.50 (at 25 FPS) | 2.20 (at 25 FPS) | 2.46 (at 23 FPS) |
| Neko, tuned                             | 1.90 | 2.28             | 2.30             | 2.52             |
| noVNC                                   | 0.01 | 0.55 (at 8 FPS)  | 0.58 (at 7 FPS)  | 0.64 (at 6 FPS)  |

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
| `--convert-threads`, `--coalesce-us`                           | `4`, `0`                           | tuning                                                                            |

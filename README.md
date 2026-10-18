# voxl

Low-latency desktop streaming for the browser, written in Rust. voxl streams an X11
display as H.264 over a WebSocket to a small web client that decodes with WebCodecs,
with input, clipboard, audio, microphone, file transfer and dynamic resolution. It
powers the Computer preview in Tembo, replacing
[selkies](https://github.com/selkies-project/selkies).

## Benchmarks

Tembo VM (16 vCPU AMD EPYC 9334, no GPU), 2560x1600 Xvfb desktop, same apps and
decoding client. FPS counts unique frames displayed; latency runs from the content
being drawn to the frame being decoded. To reproduce, see [`bench/`](bench/README.md).

### Against selkies

As deployed in Tembo, picom on.

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

### Against KasmVNC

KasmVNC 1.4.0 with Tembo's `kasmvnc` settings (60 FPS cap, JPEG/WebP quality 8–9,
video mode), no compositor.

| Test                           | KasmVNC: FPS (worst second), latency p50/p99, Mbps | voxl                           |
| ------------------------------ | -------------------------------------------------- | ------------------------------ |
| Chromium scroll, LAN           | 51.7 (39.5), 99/135 ms, 32.7                       | **60.1 (59.7), 72/81 ms, 3.6** |
| Scroll, 10 Mbps / 40 ms RTT    | 8.3 (7.4), 229/249 ms                              | **60.0 (59.1), 98/107 ms**     |
| Scroll, 5 Mbps / 60 ms RTT     | 4.9 (3.6), 306/415 ms                              | **60.1 (59.5), 129/154 ms**    |
| 1080p animation, LAN           | 56.7 (51.6), 51/82 ms, 2.0                         | **60.1 (59.9), 12/16 ms**, 7.3 |
| Animation, 10 Mbps / 40 ms RTT | 52.9 (49.2), 76/115 ms                             | **60.1 (59.5), 46/71 ms**      |
| Animation, 5 Mbps / 60 ms RTT  | 34.1 (26.4), 104/156 ms                            | **60.0 (58.8), 66/92 ms**      |
| Full-screen animation, LAN     | 34.2 (32.5), 83/102 ms, 1.9                        | **59.8 (54.0), 18/25 ms**, 5.1 |

| Server CPU, X server + streamer (cores) | idle | 1080p animation | scroll   | full screen      |
| --------------------------------------- | ---- | --------------- | -------- | ---------------- |
| KasmVNC                                 | 0.00 | 1.97            | 2.07     | 1.82 (at 34 FPS) |
| voxl                                    | 0.00 | **1.39**        | **1.38** | 1.83 (at 60 FPS) |

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

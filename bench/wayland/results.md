# Wayland capture prototype: measured results

November 5, 2026. Implemented on voxl base `c0f1ac06a61f998345ca0d4e4acc4f28fd8937fc`,
branch `tembo/wayland-capture-prototype`. These measurements are from the prototype
working tree, not upstream voxl. Tested release binary SHA-256: `bde42a28129e3711b3954c1408391d1b32774c09ee79fee8712c134794576701`.

**The native CPU Wayland path works and preserves sparse-update performance, but
it does not yet match direct-Xvfb scrolling performance.** Both backends passed the
same pixel, damage, backpressure, screenshot, and `/wait` checks. The extra cost of
observing an active desktop without viewers is measurable.

## What was implemented

An explicit `--capture-backend=wayland` adapter uses
`ext-image-copy-capture-v1` with two reusable SHM buffers. It tracks stale regions
per capture buffer, retains source damage across skipped frames, and feeds voxl's
existing partial conversion, x264, flow control and browser protocol. X11 remains
the default and retains direct access to Xvfb's shared framebuffer.

The prototype is read-only with an embedded cursor. Input, clipboard, separate
cursor delivery, portal/PipeWire, and wlr-screencopy fallback are not implemented.
External output resizing is supported; viewer-requested resizing is disabled.
Rotated/flipped captures and unsupported formats fail explicitly. Compositor
reconnect recovery remains future work.

## Experiment

- Same sandbox: 4 vCPUs, about 8 GiB RAM, no exposed GPU device.
- 2560×1600 output, target 60 fps; labwc/wlroots 0.20.2, headless/Pixman.
- Xvfb 21.1.20 on a separate display, using `-shmem`; the voxl log confirmed direct
  framebuffer access. The existing shared desktop was not used for the workload.
- Identical native C renderer and pixels, submitted through X11 MIT-SHM or native
  Wayland SHM. Wayland was not tested through XWayland.
- Software x264, CRF 24, one encoder thread, four conversion threads, one stripe,
  30 Mbps rate ceiling/floor, no audio. No compositor debug logging.
- Two passes per backend/workload, reversing backend order in the second pass.
  Each sample has 4 seconds without viewers, 4 seconds of viewer warmup, then
  12 seconds of streaming measurement. Twelve samples total.
- Localhost PyAV decoder on the same VM, ACK after decode, source pixel timestamp marker.
  Latency is **draw-to-decoded**, not browser input-to-photon.
- CPU below is **voxl + compositor/X server**, in percent of one CPU. Thus 100%
  means one fully occupied core. Source-renderer CPU is separately recorded in
  the raw samples; decoder CPU is excluded.

## Streaming results

Ranges show the two observed runs; they are not confidence intervals.

| Workload | Backend | Unique decoded fps | Latency p50 | Latency p99 | Server + compositor CPU |
| --- | --- | ---: | ---: | ---: | ---: |
| Sparse 480×48 updates | X11 | 59.90–59.94 | 13 ms | 21–22 ms | 56.40–57.02% |
| Sparse 480×48 updates | Wayland | 59.92–60.05 | 13–14 ms | 25–29 ms | 63.47–67.76% |
| Full-screen scrolling texture | X11 | 59.72–59.88 | 19–20 ms | 46–53 ms | 110.83–111.67% |
| Full-screen scrolling texture | Wayland | 54.90–54.93 | 31–32 ms | 55–57 ms | 113.11–114.55% |
| Static after refinement | X11 | No new frames | — | — | 0.08–0.17% |
| Static after refinement | Wayland | No new frames | — | — | 0.17% |

Sparse conversion remained approximately 0.07 ms per encoded frame; full-screen
conversion was approximately 1.2 ms. The adapter preserves partial conversion and
unchanged-block hints instead of converting the whole screen for every small edit.

For scrolling, Wayland delivered about 8% fewer frames at similar server CPU,
with roughly 12 ms higher median latency. This is a result of this prototype and
headless compositor combination; it does not establish an inherent Wayland limit.
Capture/compositor pacing and synchronization are the next profiling targets.

## No-viewer cost

Neither backend encoded any video in these intervals. `/wait` still observed
activity. The source application kept drawing at the requested rate.

| Workload without viewers | X11 server + compositor CPU | Wayland server + compositor CPU |
| --- | ---: | ---: |
| Static | 0 recorded CPU ticks | 0 recorded CPU ticks |
| Sparse updates | 1.50% | 13.74–14.24% |
| Scrolling | 12.74–12.99% | 21.74% |

The four-second static samples are below the CPU counter's useful resolution;
this is not a claim of literally zero work. The active-screen difference confirms
the design risk from the source review: XDamage can observe changes without
copying images, while this Wayland adapter keeps capture active to obtain damage.
Per-buffer damage hints permit reduced copying, but these measurements do not
isolate whether labwc spends the extra time in composition, copying, or scheduling.

## Correctness and build checks

Both backends passed the same integration checks:

- Screenshot pixels and their activity sequence; regional `/wait` observes the
  changed region and times out for an unrelated region.
- Alternating capture buffers preserve changes in two distant patches.
- Activity tracking works without viewers and does not encode video.
- H.264 decodes to the expected pixels.
- Withheld ACKs actually trigger backpressure; after resuming, decoded pixels
  include intervening changes rather than stale regions.
- A static screen stops encoding after paintover.

Wayland also recovered screenshots after externally resizing the output from
2560×1600 to 1920×1080 and back. Capture runs without an X11 display connection. An older labwc 0.8.4 session
without ext capture was rejected cleanly rather than silently capturing X11.

Rust formatting, release build, package verification, web formatting/lint/type
checks, and 18 unit tests passed;
one pre-existing encoder microbenchmark was ignored. Clippy passed with two
pre-existing lints allowed for the sandbox's Rust 1.96 toolchain:
`manual_is_multiple_of` in `src/audio.rs` and `manual_checked_ops` in
`src/desktop.rs`. Those unrelated files were not changed. No macOS/Windows runtime
checks or hosted CI runs are claimed.

## Recommendation

Continue with the external-compositor approach. It is already viable for sparse
interactive desktops, and the existing x264 pipeline carries over cleanly. Before
expanding to portal/GPU support, profile the roughly 55 fps scrolling limit and
active no-viewer compositor work. Then add native input/clipboard/cursor services
and compatibility fallbacks as separate changes.

This is a narrow feasibility benchmark: one VM, one compositor/renderer, two short
samples per case, synthetic drawing, and a native decoder. It does not establish
browser latency, image-quality equivalence, multi-session density, or production
readiness. Reproduction is in `bench/wayland/README.md`; all twelve sample records
are in `bench/wayland/results.json`.

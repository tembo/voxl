# Live display selection on macOS

When a Mac has multiple displays, its viewer toolbar includes a **Display** picker.
Select another display to keep the same connection and URL while voxl rebuilds capture
and sends the new geometry and a keyframe. Touch devices use a native select menu.
`--display` continues to choose the initial display.

One voxl server has one active display. A selection changes it for **all viewers**,
including read-only viewers. Read-only viewers cannot select a display, and their
toolbar does not show the picker. Linux and Windows behavior is unchanged.

Input pauses while switching. Previously queued input is drained on the old geometry,
held keys and buttons are released, and each viewer must acknowledge the new geometry
before its input is accepted. The input transform includes the selected display's
global origin and capture scale, including monitors left of or above the main one.
Decoders reset even when two displays have identical dimensions.

If a requested display disappears before capture can start, voxl tries the previous
display and then the main display, reporting the failure in the viewer. If none is
usable the server exits, as with other unrecoverable capture failures. The display
list refreshes when the pipeline rebuilds; restart voxl to discover newly attached
displays when no rebuild has occurred. Simultaneously merging displays into one
canvas and independently selecting a display per viewer are outside this feature.

## WebSocket protocol

A macOS `hello` includes an optional `displays` object:

```json
{
	"available": [
		{
			"id": 10,
			"name": "Display 1",
			"width": 1920,
			"height": 1080,
			"main": true
		}
	],
	"selected": 10,
	"generation": 0,
	"switching": false
}
```

IDs are CoreGraphics display IDs; dimensions describe desktop points, while the
outer `hello.w` and `hello.h` describe streamed pixels.

- `ds,<id>` requests a display. An invalid ID returns a `display-error` message.
- `display-switching` announces the input pause to all viewers.
- A fresh `hello` confirms the selected display, geometry and incremented generation.
- `da,<generation>` acknowledges that geometry. Acknowledgements during a pending
  switch, and input with an old or unacknowledged generation, are ignored.
- `display-error` contains a human-readable `message` and leaves or restores a usable
  display when possible.

The bundled client acknowledges automatically. Custom clients that send input must
send `da,<generation>` after processing each macOS `hello`. Before the first switch,
generation zero remains compatible with existing clients. Read-only mode still
rejects `ds` regardless of what a custom client sends.

## Validation

Run the repository's usual Rust and web checks. The CI toolchain is Rust 1.93.0.

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
npm ci
npm run check
```

With two displays attached, and Screen Recording permission for the launching
terminal, the smoke test starts temporary authenticated loopback servers. It checks
eight live switches, two viewers, new keyframes, PNG dimensions, invalid requests,
authentication and read-only protection. No screenshots or credentials are saved.
The optional pointer check moves the cursor through voxl and restores its location;
it also requires Accessibility permission for the launching terminal.

```sh
uv venv
uv pip install websockets
cargo build --release --locked
.venv/bin/python bench/display_smoke.py target/release/voxl --check-pointer
```

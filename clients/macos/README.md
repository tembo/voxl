# Voxl Viewer

A native macOS viewer for [voxl](https://github.com/tembo/voxl): a Mac app that opens
voxl's stream in its own window, with every keyboard shortcut going to the remote
computer (tembo/voxl#112).

It wraps voxl's own web client in a WKWebView, so every voxl feature (video, audio,
clipboard, files, microphone, display picker) works as it does in a browser, and adds
what a browser can't do:

- **Keyboard capture.** ⌘W, ⌘T, ⌘Q… go to the remote computer instead of closing the
  window. With Accessibility permission, system shortcuts do too: ⌘Tab, ⌘Space,
  Raycast's and Alfred's hotkeys, Mission Control, screenshot keys. ⌃⌥K switches
  between the remote computer and this Mac.
- **A native window.** voxl's floating toolbar becomes the window's toolbar (display,
  microphone, files, keyboard, full screen), with a live connection badge. Windows keep
  the remote screen's shape.
- **Graceful stops.** When voxl stops or the computer sleeps, the window shows why over
  a blurred last frame, and reconnects on its own when it's back.
- **Your computers.** A gallery of saved computers with screen previews and live status,
  plus every computer on your Tailscale tailnet, marked ready or with setup steps.
  Passwords live in the Keychain.
- **Clipboard both ways**, including images, without waiting for a click.

## Build

Needs macOS 14 or later and Swift 6 (Xcode or the Command Line Tools).

```sh
./build.sh                 # builds "build/Voxl Viewer.app"
open "build/Voxl Viewer.app"
```

voxl itself has to be reachable over HTTPS (browsers only decode video on secure pages):
`tailscale serve --https=6443 http://127.0.0.1:6901`, `voxl --tls`, or a TLS proxy.

## Test

```sh
VOXL_PASSWORD=… "build/Voxl Viewer.app/Contents/MacOS/VoxlViewer" --selftest https://host:6443
```

opens a viewer and prints JSON: whether video decodes (WebCodecs in WKWebView), and
whether ⌘W, ⌘Space and ⌘Tab reach the page exactly once without closing the window or
opening Spotlight/Raycast. Test keys are caught before voxl sees them, so nothing is typed
remotely. `--open <link>` opens a viewer for voxl's sign-in link; `--add <link>` saves one.

## How it's built

| File                    | What it does                                                                   |
| ----------------------- | ------------------------------------------------------------------------------ |
| `ViewerWindow.swift`    | A viewer window: WKWebView, connection state, sign-in, certificates, downloads |
| `ViewerChrome.swift`    | The overlay, toolbar and connection badge                                      |
| `KeyCapture.swift`      | Routing ⌘ and system shortcuts to the page                                     |
| `Health.swift`          | `/healthz` checks behind the overlay and status dots                           |
| `ConnectionsView.swift` | The gallery, sheets and settings                                               |
| `Discovery.swift`       | Tailnet computers via the Tailscale CLI, and voxl probes                       |
| `Connection.swift`      | Saved computers, Keychain, preferences                                         |
| `Thumbnails.swift`      | Screen previews from voxl's `/screenshot`                                      |

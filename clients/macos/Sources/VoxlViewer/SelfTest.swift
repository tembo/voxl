import AppKit
import WebKit

/// `VoxlViewer --selftest <url> [password]` (or VOXL_PASSWORD): opens a viewer, waits for
/// the stream, and prints what works as JSON:
///
/// - the page: secure context, WebCodecs, audio worklets, frames per second;
/// - keyboard, captured: ⌘W reaches the page exactly once and doesn't close the window;
/// - keyboard, released (⌃⌥K): ⌘W closes the window instead.
///
/// The test's keys are caught before voxl's own listeners, so nothing is typed on the
/// remote computer.
enum SelfTest {
    static func run() {
        let args = CommandLine.arguments
        guard let i = args.firstIndex(of: "--selftest"), i + 1 < args.count else {
            FileHandle.standardError.write("usage: VoxlViewer --selftest <url> [password]\n".data(using: .utf8)!)
            exit(2)
        }
        let (url, token) = Connection.split(link: args[i + 1])
        let password = i + 2 < args.count ? args[i + 2]
            : token ?? ProcessInfo.processInfo.environment["VOXL_PASSWORD"]

        let app = NSApplication.shared
        app.setActivationPolicy(.regular)
        app.mainMenu = testMenu()
        KeyCapture.shared.start()

        let v = ViewerWindowController(connection: Connection(name: "Self-test", url: url), password: password,
                                       frame: NSRect(x: 0, y: 0, width: 960, height: 600))
        v.showWindow(nil)
        app.activate(ignoringOtherApps: true)
        v.window?.makeKeyAndOrderFront(nil)
        v.load()

        var result: [String: Any] = [:]
        func js(_ s: String, _ done: @escaping (Any?) -> Void) {
            v.webView.evaluateJavaScript(s) { r, e in
                if let e { result["jsError"] = e.localizedDescription }
                done(r)
            }
        }
        /// Prints the observations, then the verdict; exits 1 if any check failed.
        func finish() {
            let data = try! JSONSerialization.data(withJSONObject: result, options: [.prettyPrinted, .sortedKeys])
            print(String(data: data, encoding: .utf8)!)
            func keys(_ k: String) -> [String] {
                guard let s = result[k] as? String, let d = s.data(using: .utf8) else { return [] }
                return (try? JSONDecoder().decode([String].self, from: d)) ?? []
            }
            func once(_ list: [String], _ key: String) -> Bool { list.filter { $0 == key }.count == 1 }
            let page = result["page"] as? [String: Any] ?? [:]
            var checks: [(String, Bool)] = [
                ("video decodes (WebCodecs)", page["videoDecoder"] as? Bool == true),
                ("stream is live", result["state"] as? String == "live"),
                ("⌘W reaches the page once", once(keys("captured.pageKeys"), "keydown:Meta+w")),
                ("⌘W keeps the window open while captured", result["captured.windowOpen"] as? Bool == true),
                ("⌘W closes the window once released", result["released.windowOpen"] as? Bool == false),
            ]
            if result["tapInstalled"] as? Bool == true {
                let sys = keys("system.pageKeys")
                checks += [
                    ("⌘Space reaches the page once", once(sys, "keydown:Meta+ ")),
                    ("⌘Tab reaches the page once", once(sys, "keydown:Meta+Tab")),
                    ("Raycast stays closed", result["system.raycastOpened"] as? Bool == false),
                ]
            } else {
                print("(system shortcuts not checked: no Accessibility permission)")
            }
            for (name, ok) in checks { print(ok ? "✓" : "✗", name) }
            exit(checks.allSatisfy(\.1) ? 0 : 1)
        }
        /// What a person does: press ⌘, press and release W, release ⌘.
        func pressCmdW() {
            let wn = v.window?.windowNumber ?? 0
            let presses: [(NSEvent.EventType, String, NSEvent.ModifierFlags, UInt16)] = [
                (.flagsChanged, "", .command, 55),
                (.keyDown, "w", .command, 13), (.keyUp, "w", .command, 13),
                (.flagsChanged, "", [], 55),
            ]
            for (type, chars, mods, code) in presses {
                if let e = NSEvent.keyEvent(with: type, location: .zero, modifierFlags: mods,
                                            timestamp: ProcessInfo.processInfo.systemUptime,
                                            windowNumber: wn, context: nil, characters: chars,
                                            charactersIgnoringModifiers: chars, isARepeat: false, keyCode: code) {
                    NSApp.postEvent(e, atStart: false)
                }
            }
        }
        /// Posts key presses at the HID level, like a real keyboard: modifier down, key
        /// down and up, modifier up.
        func postSystem(_ keys: [(CGKeyCode, CGEventFlags)]) {
            let src = CGEventSource(stateID: .hidSystemState)
            for (code, flags) in keys {
                let mod: CGKeyCode = flags.contains(.maskCommand) ? 55 : 58
                let md = CGEvent(keyboardEventSource: src, virtualKey: mod, keyDown: true)
                md?.flags = flags
                md?.post(tap: .cghidEventTap)
                for down in [true, false] {
                    let e = CGEvent(keyboardEventSource: src, virtualKey: code, keyDown: down)
                    e?.flags = flags
                    e?.post(tap: .cghidEventTap)
                }
                let mu = CGEvent(keyboardEventSource: src, virtualKey: mod, keyDown: false)
                mu?.flags = []
                mu?.post(tap: .cghidEventTap)
            }
        }
        func raycastWindows() -> Int {
            let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as? [[String: Any]] ?? []
            return list.filter { ($0[kCGWindowOwnerName as String] as? String) == "Raycast" }.count
        }
        func after(_ s: Double, _ f: @escaping () -> Void) {
            DispatchQueue.main.asyncAfter(deadline: .now() + s, execute: f)
        }

        after(7) {
            js("""
            JSON.stringify({
              url: location.origin,
              secureContext: isSecureContext,
              videoDecoder: 'VideoDecoder' in window,
              offscreenCanvas: !!HTMLCanvasElement.prototype.transferControlToOffscreen,
              audioWorklet: typeof AudioWorkletNode,
              fps: window.fps,
              stats: window.voxlStats,
              clipboardShim: navigator.clipboard?.constructor === Object,
            })
            """) { r in
                if let s = r as? String, let d = s.data(using: .utf8),
                   let o = try? JSONSerialization.jsonObject(with: d) { result["page"] = o }
                js("""
                window.__keys = [];
                for (const t of ['keydown', 'keyup'])
                  window.addEventListener(t, (e) => {
                    window.__keys.push(t + ':' + (e.metaKey ? 'Meta+' : '') + e.key);
                    e.stopImmediatePropagation(); e.preventDefault();
                  }, true);
                true
                """) { _ in
                    result["windowIsKey"] = v.window?.isKeyWindow ?? false
                    pressCmdW()
                    after(1) {
                        js("JSON.stringify(window.__keys)") { r in
                            result["captured.pageKeys"] = r ?? NSNull()
                            result["captured.windowOpen"] = v.window?.isVisible ?? false
                            result["state"] = v.model.state == .live ? "live" : "\(v.model.state)"
                            result["accessibility"] = KeyCapture.shared.accessibilityGranted
                            result["tapInstalled"] = KeyCapture.shared.isTapInstalled
                            // System shortcuts, posted where real keystrokes enter macOS.
                            let raycastBefore = raycastWindows()
                            js("window.__keys = []; true") { _ in
                                postSystem([(49, .maskCommand), (48, .maskCommand)]) // ⌘Space, ⌘Tab
                                after(1.5) {
                                    js("JSON.stringify(window.__keys)") { r in
                                        result["system.pageKeys"] = r ?? NSNull()
                                        result["system.raycastOpened"] = raycastWindows() > raycastBefore
                                        result["system.stillFrontmost"] = NSApp.isActive
                                        v.captureActive = false
                                        pressCmdW()
                                        after(1) {
                                            result["released.windowOpen"] = v.window?.isVisible ?? false
                                            finish()
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        app.run()
    }

    /// The menus that matter for the test: Close (⌘W) and Paste (⌘V), as in the app.
    private static func testMenu() -> NSMenu {
        let main = NSMenu()
        for (title, items) in [("File", [("Close", "w", #selector(NSWindow.performClose(_:)))]),
                               ("Edit", [("Paste", "v", #selector(NSText.paste(_:)))])] {
            let top = NSMenuItem(title: title, action: nil, keyEquivalent: "")
            let menu = NSMenu(title: title)
            for (t, k, sel) in items { menu.addItem(withTitle: t, action: sel, keyEquivalent: k) }
            top.submenu = menu
            main.addItem(top)
        }
        return main
    }
}

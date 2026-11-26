import AppKit
import ApplicationServices

/// Sends keyboard shortcuts to the remote computer instead of this Mac.
///
/// Two layers:
///
/// - **App shortcuts** (⌘W, ⌘T, ⌘Q…): a local event monitor makes sure they reach
///   voxl's page rather than this app's menus. No permission needed.
/// - **System shortcuts** (⌘Tab, ⌘Space, Raycast's or Alfred's hotkey, Spaces, screenshot
///   keys…): macOS hands these to other apps before this one ever sees them. With
///   Accessibility permission, an event tap takes every key event while a viewer is in
///   front and posts it straight back to this app, which skips the system's hotkey
///   matching, so they go to the remote computer too.
///
/// ⌃⌥K releases the keyboard (and takes it back).
final class KeyCapture {
    static let shared = KeyCapture()

    private var monitor: Any?
    private var tap: CFMachPort?
    private var tapSource: CFRunLoopSource?

    /// Marks events this app re-posted to itself, so the tap lets them through.
    private static let marker: Int64 = 0x766F_786C // "voxl"

    private enum Key {
        static let k: UInt16 = 40
    }

    func start() {
        monitor = NSEvent.addLocalMonitorForEvents(matching: [.keyDown, .keyUp]) { [weak self] e in
            // nil means "handled": don't let `?? e` turn it back into the event.
            guard let self else { return e }
            return self.handle(e)
        }
        let nc = NotificationCenter.default
        nc.addObserver(forName: NSApplication.didBecomeActiveNotification, object: nil, queue: .main) {
            [weak self] _ in self?.refresh()
        }
        nc.addObserver(forName: UserDefaults.didChangeNotification, object: nil, queue: .main) {
            [weak self] _ in self?.refresh()
        }
        // Accessibility can be granted while the app runs; notice within a few seconds.
        Timer.scheduledTimer(withTimeInterval: 3, repeats: true) { [weak self] _ in
            guard let self, Prefs.captureSystemShortcuts, self.tap == nil else { return }
            self.refresh()
        }
        refresh()
    }

    /// Re-reads preferences and permissions.
    func refresh() {
        updateTap()
        ViewerWindowController.all.forEach { $0.syncModel() }
    }

    private static func flags(_ e: NSEvent) -> NSEvent.ModifierFlags {
        e.modifierFlags.intersection(.deviceIndependentFlagsMask)
            .subtracting([.capsLock, .numericPad, .function])
    }

    /// Returns the event to let it through, or nil once it's been dealt with.
    func handle(_ e: NSEvent) -> NSEvent? {
        guard let viewer = ViewerWindowController.key else { return e }
        let flags = Self.flags(e)
        if flags == [.control, .option], e.keyCode == Key.k {
            if e.type == .keyDown && !e.isARepeat { viewer.captureActive.toggle() }
            return nil
        }
        guard flags.contains(.command) else { return e }

        if !viewer.isCapturing {
            // Released, or the stream isn't showing: ⌘ shortcuts are this Mac's. The page
            // would otherwise take them (voxl handles every key), so menus go first.
            if e.type == .keyDown, NSApp.mainMenu?.performKeyEquivalent(with: e) == true { return nil }
            return e
        }
        // Captured. A ⌘ keydown takes AppKit's normal path: the web view gets it as a
        // key equivalent before the menus, and since voxl's page handles it, no menu
        // item runs. (Handing it to the web view directly makes WebKit dispatch it
        // twice.) ⌘V is no different: voxl leaves it unhandled, so the Edit menu's
        // Paste gives the page this Mac's clipboard before voxl presses ⌘V remotely.
        if e.type == .keyDown { return e }
        // AppKit drops keyups of keys released while ⌘ is held; deliver them here.
        viewer.webView.keyUp(with: e)
        return nil
    }

    // MARK: System shortcuts

    var accessibilityGranted: Bool { AXIsProcessTrusted() }
    var isTapInstalled: Bool { tap != nil }

    /// Asks macOS for Accessibility, showing its prompt the first time.
    func requestAccessibility() {
        let opt = kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String
        _ = AXIsProcessTrustedWithOptions([opt: true] as CFDictionary)
    }

    func updateTap() {
        let want = Prefs.captureAppShortcuts && Prefs.captureSystemShortcuts && AXIsProcessTrusted()
        if want, tap == nil { installTap() } else if !want, tap != nil { removeTap() }
    }

    private func installTap() {
        // Modifier changes too, so they stay in order with the keys they go with.
        let mask = (1 << CGEventType.keyDown.rawValue) | (1 << CGEventType.keyUp.rawValue)
            | (1 << CGEventType.flagsChanged.rawValue)
        guard let tap = CGEvent.tapCreate(
            tap: .cgSessionEventTap, place: .headInsertEventTap, options: .defaultTap,
            eventsOfInterest: CGEventMask(mask),
            callback: { _, type, event, _ in
                KeyCapture.shared.tapped(type, event)
            },
            userInfo: nil
        ) else { return }
        self.tap = tap
        tapSource = CFMachPortCreateRunLoopSource(nil, tap, 0)
        CFRunLoopAddSource(CFRunLoopGetMain(), tapSource, .commonModes)
        CGEvent.tapEnable(tap: tap, enable: true)
    }

    private func removeTap() {
        if let tap { CGEvent.tapEnable(tap: tap, enable: false) }
        if let tapSource { CFRunLoopRemoveSource(CFRunLoopGetMain(), tapSource, .commonModes) }
        tap = nil
        tapSource = nil
    }

    /// Runs on the main run loop for every key event in the session: keep it cheap.
    private func tapped(_ type: CGEventType, _ event: CGEvent) -> Unmanaged<CGEvent>? {
        if type == .tapDisabledByTimeout || type == .tapDisabledByUserInput {
            if let tap { CGEvent.tapEnable(tap: tap, enable: true) }
            return Unmanaged.passUnretained(event)
        }
        guard type == .keyDown || type == .keyUp || type == .flagsChanged,
              event.getIntegerValueField(.eventSourceUserData) != Self.marker,
              NSApp.isActive, let viewer = ViewerWindowController.key, viewer.isCapturing
        else { return Unmanaged.passUnretained(event) }
        // Straight to this app: no hotkey matching on the way, and from here on the
        // event takes the same path as any other key.
        event.setIntegerValueField(.eventSourceUserData, value: Self.marker)
        event.postToPid(ProcessInfo.processInfo.processIdentifier)
        return nil
    }
}

import AppKit
import SwiftUI
import WebKit

/// One window per remote computer, showing voxl's own web client in a WKWebView.
///
/// The window keeps track of the connection itself, so a stopped or sleeping server
/// shows a clear overlay and reconnects on its own instead of a blank or frozen page.
final class ViewerWindowController: NSWindowController, NSWindowDelegate {
    private static var open: [UUID: ViewerWindowController] = [:]
    static var all: [ViewerWindowController] { Array(open.values) }
    static var key: ViewerWindowController? {
        NSApp.keyWindow?.windowController as? ViewerWindowController
    }

    /// A saved computer changed: an open viewer picks up the new address or password.
    static func update(_ c: Connection, password: String?) {
        guard let v = open[c.id] else { return }
        let reconnect = v.connection.url != c.url || v.password != password
        v.connection = c
        v.password = password
        v.window?.title = c.name
        v.model.name = c.name
        v.model.host = c.host
        if reconnect { v.load() }
    }

    /// Opens a viewer for the connection, or brings its window forward.
    static func show(_ c: Connection, password: String? = nil) {
        if let v = open[c.id] {
            v.window?.makeKeyAndOrderFront(nil)
            return
        }
        let v = ViewerWindowController(connection: c, password: password ?? c.password)
        open[c.id] = v
        v.showWindow(nil)
        v.load()
    }

    private(set) var connection: Connection
    private var password: String?
    let webView: WKWebView
    let model: ViewerModel
    private let bridge = ScriptBridge()
    private var pendingTrust: [(URLSession.AuthChallengeDisposition, URLCredential?) -> Void] = []
    private var downloads: [ObjectIdentifier: URL] = [:]
    private var pollTask: Task<Void, Never>?
    private var reconnectingSince: Date?
    private var closed = false
    /// Bumped on every load, so late results of an earlier attempt are ignored.
    private var generation = 0
    private var toolbarController: ViewerToolbar!
    private var statsTimer: Timer?
    /// The remote screen's shape, once known; the window keeps to it.
    private var aspect: NSSize?

    /// Whether ⌘ shortcuts go to the remote computer (⌃⌥K toggles).
    var captureActive = true { didSet { syncModel() } }
    /// Keys go to the remote computer only while it's on screen; with the overlay up,
    /// ⌘W and friends work on this Mac.
    var isCapturing: Bool { captureActive && Prefs.captureAppShortcuts && model.state == .live }

    init(connection: Connection, password: String?, frame: NSRect = NSRect(x: 0, y: 0, width: 1440, height: 900)) {
        self.connection = connection
        self.password = password
        model = ViewerModel(name: connection.name, host: connection.host)

        let config = WKWebViewConfiguration()
        // The page's URL carries the password as ?token=: keep nothing on disk.
        config.websiteDataStore = .nonPersistent()
        config.mediaTypesRequiringUserActionForPlayback = []
        let ucc = config.userContentController
        ucc.addUserScript(WKUserScript(source: ScriptBridge.clipboardShim,
                                       injectionTime: .atDocumentStart, forMainFrameOnly: true))
        ucc.addUserScript(WKUserScript(source: ScriptBridge.statusWatcher,
                                       injectionTime: .atDocumentStart, forMainFrameOnly: true))
        ucc.addUserScript(WKUserScript(source: ScriptBridge.nativeChrome,
                                       injectionTime: .atDocumentEnd, forMainFrameOnly: true))
        ucc.add(bridge, name: ScriptBridge.clipboardHandler)
        ucc.add(bridge, name: ScriptBridge.statusHandler)

        webView = WKWebView(frame: frame, configuration: config)
        webView.allowsBackForwardNavigationGestures = false
        webView.allowsMagnification = false
        webView.underPageBackgroundColor = .black
        webView.setValue(false, forKey: "drawsBackground")
        if #available(macOS 13.3, *) { webView.isInspectable = true }

        let window = NSWindow(contentRect: frame,
                              styleMask: [.titled, .closable, .miniaturizable, .resizable, .unifiedTitleAndToolbar],
                              backing: .buffered, defer: false)
        window.backgroundColor = .black
        window.collectionBehavior.insert(.fullScreenPrimary)
        window.title = connection.name
        window.isReleasedWhenClosed = false
        window.tabbingMode = .disallowed

        // The web view, with the overlay above it.
        let container = NSView(frame: frame)
        webView.frame = container.bounds
        webView.autoresizingMask = [.width, .height]
        container.addSubview(webView)
        let overlay = NSHostingView(rootView: ViewerOverlay(model: model))
        overlay.frame = container.bounds
        overlay.autoresizingMask = [.width, .height]
        container.addSubview(overlay)
        window.contentView = container

        window.minSize = NSSize(width: 480, height: 320)
        window.center()
        window.setFrameAutosaveName("Viewer \(connection.id.uuidString)")

        super.init(window: window)
        let tb = ViewerToolbar(model: model)
        toolbarController = tb
        window.toolbar = tb.toolbar
        window.toolbarStyle = .unified
        window.subtitle = model.state.subtitle
        window.delegate = self
        bridge.owner = self
        webView.navigationDelegate = self
        webView.uiDelegate = self

        model.onClose = { [weak self] in self?.close() }
        model.onRetry = { [weak self] in self?.load() }
        model.backdrop = Thumbnails.shared.images[connection.id]
        tb.onToggleCapture = { [weak self] in self?.captureActive.toggle() }
        tb.onFullScreen = { [weak self] in self?.window?.toggleFullScreen(nil) }
        tb.onMic = { [weak self] in self?.click("mic-btn") }
        tb.onFiles = { [weak self] in self?.click("files-btn") }
        tb.onDisplay = { [weak self] id in
            let safe = id.filter { $0.isNumber }
            self?.webView.evaluateJavaScript("document.querySelector('#display-menu .option[data-id=\"\(safe)\"]')?.click()")
        }
        syncModel()
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    /// Keeps the title bar in step with preferences and permissions.
    func syncModel() {
        model.capturing = captureActive
        model.appShortcuts = Prefs.captureAppShortcuts
        model.systemShortcutsWanted = Prefs.captureSystemShortcuts
        model.accessibility = KeyCapture.shared.accessibilityGranted
        toolbarController?.update()
    }

    /// Presses one of voxl's own (hidden) toolbar buttons.
    private func click(_ id: String) {
        webView.evaluateJavaScript("document.getElementById('\(id)')?.click()")
    }

    // MARK: Connecting

    /// Checks the server first, so a stopped voxl shows the overlay rather than an
    /// empty error page, then loads voxl's page.
    func load() {
        guard !closed else { return }
        pollTask?.cancel()
        generation += 1
        guard let base = connection.baseURL, let url = connection.loadURL(password: password) else {
            set(.failed("“\(connection.url)” isn't a valid address. Edit the computer to fix it."))
            return
        }
        // Plain HTTP to another machine would send the password in the clear, and the
        // video wouldn't play anyway (browsers only decode it on secure pages).
        guard connection.isSecureContext else {
            set(.failed("\(connection.host) is set up with plain HTTP. Serve voxl over HTTPS (for example with `tailscale serve`), then edit the address."))
            return
        }
        if model.state != .live { set(.connecting) }
        let pinned = connection.certFingerprint
        pollTask = Task { [weak self] in
            let health = await Health.check(base, pinned: pinned)
            guard !Task.isCancelled else { return }
            await MainActor.run {
                guard let self, !self.closed else { return }
                switch health {
                case .up:
                    self.webView.load(URLRequest(url: url))
                case .untrusted where pinned == nil:
                    // A self-signed certificate nobody trusted yet: the page load asks.
                    self.webView.load(URLRequest(url: url))
                default:
                    self.wait(health)
                }
            }
        }
    }

    func reload() { load() }

    /// The server is down: say so, and check again with a gentle backoff.
    private func wait(_ health: Health) {
        set(.waiting(health))
        poll(attempt: 0) { [weak self] h in
            guard let self else { return true }
            if h.isUp { self.load(); return true }
            if case .waiting = self.model.state { self.set(.waiting(h)) }
            return false
        }
    }

    /// Calls `check` with the server's health every 2–5 s until it returns true.
    private func poll(attempt: Int, _ check: @escaping (Health) -> Bool) {
        pollTask?.cancel()
        let delay = [2.0, 2, 3, 3, 5][min(attempt, 4)]
        model.nextTry = Date().addingTimeInterval(delay)
        guard let base = connection.baseURL else { return }
        let pinned = connection.certFingerprint
        pollTask = Task { [weak self] in
            try? await Task.sleep(for: .seconds(delay))
            guard !Task.isCancelled else { return }
            let h = await Health.check(base, pinned: pinned)
            guard !Task.isCancelled else { return }
            await MainActor.run {
                guard let self, !self.closed else { return }
                self.model.nextTry = nil
                if !check(h) { self.poll(attempt: attempt + 1, check) }
            }
        }
    }

    private func set(_ s: LinkState) {
        guard model.state != s else { return }
        if model.state == .live {
            // Keep the last frame to blur behind the overlay. (The snapshot comes back
            // after the overlay appears; it fades in under it.)
            webView.takeSnapshot(with: nil) { [weak self] image, _ in
                if let image { self?.model.backdrop = image }
            }
        }
        model.state = s
        window?.subtitle = s.subtitle
        if s == .live {
            pollTask?.cancel()
            model.nextTry = nil
            reconnectingSince = nil
            // Keys typed now should reach the stream.
            window?.makeFirstResponder(webView)
            startStats()
        } else {
            statsTimer?.invalidate()
            statsTimer = nil
            model.stats = StreamStats()
        }
    }

    // MARK: Stats and shape

    /// Samples the page's numbers once a second, and learns the remote screen's shape.
    private func startStats() {
        statsTimer?.invalidate()
        let t = Timer(timeInterval: 1, repeats: true) { [weak self] _ in self?.sample() }
        RunLoop.main.add(t, forMode: .common)
        statsTimer = t
        sample()
    }

    private func sample() {
        webView.evaluateJavaScript(ScriptBridge.sampleStats) { [weak self] r, _ in
            guard let self, let s = r as? String, let d = s.data(using: .utf8),
                  let o = try? JSONSerialization.jsonObject(with: d) as? [String: Any] else { return }
            func n(_ k: String) -> Double { (o[k] as? NSNumber)?.doubleValue ?? 0 }
            let stats = StreamStats(fps: n("fps"), kbps: n("kbps"), decodeMs: n("decode"), pipeMs: n("pipe"),
                                    codec: o["codec"] as? String ?? "")
            if self.model.stats != stats { self.model.stats = stats }
            let w = n("w"), h = n("h")
            if w > 0, h > 0 { self.fit(to: NSSize(width: w, height: h)) }
        }
    }

    /// Keeps the window the shape of the remote screen, so there are no black bars.
    private func fit(to size: NSSize) {
        guard let window, !window.styleMask.contains(.fullScreen) else { return }
        let ratio = size.width / size.height
        if let a = aspect, abs(a.width / a.height - ratio) < 0.01 { return }
        aspect = size
        window.contentAspectRatio = size
        fitWindow()
    }

    /// Resizes the window to the remote screen's shape, within this screen.
    func fitWindow() {
        guard let window, let aspect, let screen = window.screen ?? NSScreen.main else { return }
        let ratio = aspect.width / aspect.height
        let content = window.contentRect(forFrameRect: window.frame)
        let chrome = window.frame.height - content.height
        let visible = screen.visibleFrame
        var w = content.width
        var h = w / ratio
        if h + chrome > visible.height { h = visible.height - chrome; w = h * ratio }
        if w > visible.width { w = visible.width; h = w / ratio }
        var frame = window.frameRect(forContentRect: NSRect(x: content.minX, y: content.maxY - h, width: w, height: h))
        frame.origin.x = min(max(frame.minX, visible.minX), visible.maxX - frame.width)
        frame.origin.y = min(max(frame.minY, visible.minY), visible.maxY - frame.height)
        window.setFrame(frame, display: true, animate: window.isVisible)
    }

    /// What voxl's toolbar offers (see ScriptBridge.statusWatcher).
    fileprivate func pageTools(_ b: [String: Any]) {
        let displays = (b["displays"] as? [[String: Any]] ?? []).map {
            PageTools.Display(id: $0["id"] as? String ?? "", name: $0["name"] as? String ?? "",
                              selected: $0["selected"] as? Bool ?? false)
        }
        let t = PageTools(mic: b["mic"] as? Bool ?? false, micOn: b["micOn"] as? Bool ?? false,
                          files: b["files"] as? Bool ?? false, readOnly: b["readOnly"] as? Bool ?? false,
                          displays: displays)
        if model.tools != t { model.tools = t }
    }

    /// What voxl's page says about its stream (see ScriptBridge.statusWatcher).
    fileprivate func pageStatus(_ state: String, _ text: String) {
        switch state {
        case "live":
            set(.live)
        case "connecting":
            if model.state != .live { set(.connecting) }
        case "reconnecting":
            set(.reconnecting)
            reconnectingSince = reconnectingSince ?? Date()
            // The page retries the stream itself. Meanwhile, find out why, and if the
            // server is back but the page still hasn't caught up, start over.
            poll(attempt: 0) { [weak self] h in
                guard let self, self.model.state == .reconnecting else { return true }
                if h.isUp {
                    if let since = self.reconnectingSince, Date().timeIntervalSince(since) > 8 {
                        self.load()
                        return true
                    }
                    return false
                }
                self.wait(h)
                return true
            }
        default:
            set(.failed(text.isEmpty ? "voxl's page reported an error." : text))
        }
    }

    // MARK: Window

    func windowDidBecomeKey(_ notification: Notification) { syncModel() }

    func windowWillClose(_ notification: Notification) {
        // Keep the last picture for the gallery.
        if model.state == .live, ConnectionStore.shared.connection(connection.id) != nil {
            let id = connection.id
            webView.takeSnapshot(with: nil) { image, _ in
                guard let image else { return }
                Task { @MainActor in
                    if ConnectionStore.shared.connection(id) != nil { Thumbnails.shared.set(image, for: id) }
                }
            }
        }
        closed = true
        statsTimer?.invalidate()
        pollTask?.cancel()
        webView.stopLoading()
        let ucc = webView.configuration.userContentController
        ucc.removeScriptMessageHandler(forName: ScriptBridge.clipboardHandler)
        ucc.removeScriptMessageHandler(forName: ScriptBridge.statusHandler)
        // Leaving the page closes voxl's WebSocket right away.
        webView.loadHTMLString("", baseURL: nil)
        Self.open[connection.id] = nil
    }

    // MARK: Helpers

    /// Same scheme, host and port as the server.
    func isOwn(_ url: URL) -> Bool {
        guard let base = connection.baseURL else { return false }
        func port(_ u: URL) -> Int { u.port ?? (u.scheme == "https" ? 443 : 80) }
        return url.scheme == base.scheme && url.host == base.host && port(url) == port(base)
    }

    private func askPassword(_ done: @escaping (String?) -> Void) {
        guard let window else { return done(nil) }
        let a = NSAlert()
        a.messageText = "Password for \(connection.name)"
        a.informativeText = password == nil
            ? "Enter the password voxl was started with."
            : "voxl didn't accept the saved password."
        let field = NSSecureTextField(frame: NSRect(x: 0, y: 0, width: 260, height: 24))
        a.accessoryView = field
        a.addButton(withTitle: "Connect")
        a.addButton(withTitle: "Cancel")
        a.window.initialFirstResponder = field
        a.beginSheetModal(for: window) { r in
            done(r == .alertFirstButtonReturn && !field.stringValue.isEmpty ? field.stringValue : nil)
        }
    }

    /// voxl's --tls makes a self-signed certificate: trust it once per connection
    /// (pinned by fingerprint), the way SSH trusts a new host.
    private func handleTrust(_ ch: URLAuthenticationChallenge,
                             _ completion: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void) {
        guard let trust = ch.protectionSpace.serverTrust else {
            return completion(.performDefaultHandling, nil)
        }
        if SecTrustEvaluateWithError(trust, nil) { return completion(.performDefaultHandling, nil) }
        let fp = Health.fingerprint(trust)
        if let fp, fp == connection.certFingerprint {
            return completion(.useCredential, URLCredential(trust: trust))
        }
        pendingTrust.append(completion)
        guard pendingTrust.count == 1, let window, let fp else {
            if fp == nil { finishTrust(nil) }
            return
        }
        let a = NSAlert()
        a.alertStyle = .warning
        a.messageText = "Trust \(connection.host)'s certificate?"
        let pretty = stride(from: 0, to: fp.count, by: 2).map {
            String(fp[fp.index(fp.startIndex, offsetBy: $0)..<fp.index(fp.startIndex, offsetBy: $0 + 2)])
        }.joined(separator: ":")
        a.informativeText = """
        It isn't signed by an authority this Mac trusts. voxl's --tls makes a \
        self-signed certificate like this; continue only if you started that server.

        SHA-256: \(pretty)
        """
        a.addButton(withTitle: "Trust and Connect")
        a.addButton(withTitle: "Cancel")
        a.beginSheetModal(for: window) { [weak self] r in
            guard let self else { return }
            if r == .alertFirstButtonReturn {
                self.connection.certFingerprint = fp
                if ConnectionStore.shared.connection(self.connection.id) != nil {
                    ConnectionStore.shared.upsert(self.connection)
                }
                self.finishTrust(trust)
            } else {
                self.finishTrust(nil)
                self.set(.failed("The certificate wasn't trusted."))
            }
        }
    }

    private func finishTrust(_ trust: SecTrust?) {
        let waiting = pendingTrust
        pendingTrust = []
        for c in waiting {
            if let trust { c(.useCredential, URLCredential(trust: trust)) }
            else { c(.cancelAuthenticationChallenge, nil) }
        }
    }

    private func uniqueDownloadURL(_ name: String) -> URL {
        let dir = FileManager.default.urls(for: .downloadsDirectory, in: .userDomainMask)[0]
        let safe = name.isEmpty ? "download" : name.replacingOccurrences(of: "/", with: "-")
        var url = dir.appendingPathComponent(safe)
        let stem = url.deletingPathExtension().lastPathComponent, ext = url.pathExtension
        var n = 2
        while FileManager.default.fileExists(atPath: url.path) {
            url = dir.appendingPathComponent(ext.isEmpty ? "\(stem) \(n)" : "\(stem) \(n).\(ext)")
            n += 1
        }
        return url
    }
}

// MARK: - Navigation, sign-in and certificates

extension ViewerWindowController: WKNavigationDelegate {
    func webView(_ webView: WKWebView, decidePolicyFor action: WKNavigationAction,
                 preferences: WKWebpagePreferences,
                 decisionHandler: @escaping (WKNavigationActionPolicy, WKWebpagePreferences) -> Void) {
        if action.shouldPerformDownload { return decisionHandler(.download, preferences) }
        guard let url = action.request.url else { return decisionHandler(.allow, preferences) }
        if isOwn(url) || ["about", "blob", "data"].contains(url.scheme ?? "") {
            return decisionHandler(.allow, preferences)
        }
        // Links to other sites open in the default browser.
        if action.targetFrame?.isMainFrame ?? true { NSWorkspace.shared.open(url) }
        decisionHandler(.cancel, preferences)
    }

    func webView(_ webView: WKWebView, decidePolicyFor response: WKNavigationResponse,
                 decisionHandler: @escaping (WKNavigationResponsePolicy) -> Void) {
        if response.isForMainFrame, let http = response.response as? HTTPURLResponse {
            switch http.statusCode {
            case 500...:
                // `tailscale serve` (or another proxy) with voxl stopped.
                decisionHandler(.cancel)
                wait(.notRunning)
                return
            case 403:
                decisionHandler(.cancel)
                set(.failed("voxl refused this address. Start it with --allowed-hosts=\(connection.host) so it answers to that name."))
                return
            case 404:
                decisionHandler(.cancel)
                set(.failed("There's a web server at \(connection.host), but it isn't voxl. Check the address and port."))
                return
            default:
                break
            }
        }
        decisionHandler(response.canShowMIMEType ? .allow : .download)
    }

    func webView(_ webView: WKWebView, navigationAction: WKNavigationAction, didBecome download: WKDownload) {
        download.delegate = self
    }

    func webView(_ webView: WKWebView, navigationResponse: WKNavigationResponse, didBecome download: WKDownload) {
        download.delegate = self
    }

    func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: Error) {
        report(error)
    }

    func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error) {
        report(error)
    }

    private func report(_ error: Error) {
        guard !closed else { return }
        let e = error as NSError
        if e.domain == NSURLErrorDomain && e.code == NSURLErrorCancelled { return }
        if e.domain == "WebKitErrorDomain" && (e.code == 102 || e.code == 101) { return } // download / policy
        // Most often the server went away between the health check and the page load.
        guard let base = connection.baseURL else { return }
        let pinned = connection.certFingerprint
        let attempt = generation
        Task { [weak self] in
            let h = await Health.check(base, pinned: pinned)
            await MainActor.run {
                guard let self, !self.closed, self.generation == attempt, self.model.state != .live else { return }
                if h.isUp { self.set(.failed(e.localizedDescription)) } else { self.wait(h) }
            }
        }
    }

    func webViewWebContentProcessDidTerminate(_ webView: WKWebView) { load() }

    func webView(_ webView: WKWebView, didReceive challenge: URLAuthenticationChallenge,
                 completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void) {
        switch challenge.protectionSpace.authenticationMethod {
        case NSURLAuthenticationMethodServerTrust:
            handleTrust(challenge, completionHandler)
        case NSURLAuthenticationMethodHTTPBasic, NSURLAuthenticationMethodDefault:
            // Only ever give the password to this server, never to another site or a proxy.
            let space = challenge.protectionSpace
            guard !space.isProxy(), let base = connection.baseURL, space.host == base.host,
                  space.port == (base.port ?? (base.scheme == "https" ? 443 : 80)),
                  space.protocol == base.scheme else {
                return completionHandler(.performDefaultHandling, nil)
            }
            if challenge.previousFailureCount == 0, let password {
                completionHandler(.useCredential,
                                  URLCredential(user: "voxl", password: password, persistence: .forSession))
                return
            }
            askPassword { [weak self] pw in
                guard let self else { return }
                completionHandler(.cancelAuthenticationChallenge, nil)
                guard let pw else {
                    self.set(.failed("voxl needs the password it was started with."))
                    return
                }
                self.password = pw
                if ConnectionStore.shared.connection(self.connection.id) != nil { self.connection.password = pw }
                // Reload so the page and its connections all carry the new token.
                DispatchQueue.main.async { self.load() }
            }
        default:
            completionHandler(.performDefaultHandling, nil)
        }
    }
}

// MARK: - File panels, microphone, pop-ups

extension ViewerWindowController: WKUIDelegate {
    func webView(_ webView: WKWebView, runOpenPanelWith parameters: WKOpenPanelParameters,
                 initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping ([URL]?) -> Void) {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = parameters.allowsDirectories
        panel.canChooseFiles = !parameters.allowsDirectories
        panel.allowsMultipleSelection = parameters.allowsMultipleSelection
        panel.prompt = "Upload"
        guard let window else { return completionHandler(nil) }
        panel.beginSheetModal(for: window) { r in
            completionHandler(r == .OK ? panel.urls : nil)
        }
    }

    func webView(_ webView: WKWebView, requestMediaCapturePermissionFor origin: WKSecurityOrigin,
                 initiatedByFrame frame: WKFrameInfo, type: WKMediaCaptureType,
                 decisionHandler: @escaping (WKPermissionDecision) -> Void) {
        // The microphone button in voxl's toolbar: allow it for this server only.
        decisionHandler(origin.host == connection.baseURL?.host ? .grant : .deny)
    }

    func webView(_ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration,
                 for action: WKNavigationAction, windowFeatures: WKWindowFeatures) -> WKWebView? {
        if let url = action.request.url { NSWorkspace.shared.open(url) }
        return nil
    }

    func webView(_ webView: WKWebView, runJavaScriptAlertPanelWithMessage message: String,
                 initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping () -> Void) {
        guard let window else { return completionHandler() }
        let a = NSAlert()
        a.messageText = message
        a.beginSheetModal(for: window) { _ in completionHandler() }
    }

    func webView(_ webView: WKWebView, runJavaScriptConfirmPanelWithMessage message: String,
                 initiatedByFrame frame: WKFrameInfo, completionHandler: @escaping (Bool) -> Void) {
        guard let window else { return completionHandler(false) }
        let a = NSAlert()
        a.messageText = message
        a.addButton(withTitle: "OK")
        a.addButton(withTitle: "Cancel")
        a.beginSheetModal(for: window) { r in completionHandler(r == .alertFirstButtonReturn) }
    }
}

// MARK: - Downloads from voxl's Files panel

extension ViewerWindowController: WKDownloadDelegate {
    func download(_ download: WKDownload, decideDestinationUsing response: URLResponse,
                  suggestedFilename: String, completionHandler: @escaping (URL?) -> Void) {
        let url = uniqueDownloadURL(suggestedFilename)
        downloads[ObjectIdentifier(download)] = url
        completionHandler(url)
    }

    func downloadDidFinish(_ download: WKDownload) {
        guard let url = downloads.removeValue(forKey: ObjectIdentifier(download)) else { return }
        // Bounces the Downloads stack in the Dock, like Safari.
        DistributedNotificationCenter.default().post(
            name: Notification.Name("com.apple.DownloadFileFinished"), object: url.path)
    }

    func download(_ download: WKDownload, didFailWithError error: Error, resumeData: Data?) {
        downloads.removeValue(forKey: ObjectIdentifier(download))
        guard let window else { return }
        NSAlert(error: error).beginSheetModal(for: window)
    }

    func download(_ download: WKDownload, didReceive challenge: URLAuthenticationChallenge,
                  completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void) {
        if challenge.protectionSpace.authenticationMethod == NSURLAuthenticationMethodServerTrust,
           let trust = challenge.protectionSpace.serverTrust,
           let fp = Health.fingerprint(trust), fp == connection.certFingerprint {
            return completionHandler(.useCredential, URLCredential(trust: trust))
        }
        completionHandler(.performDefaultHandling, nil)
    }
}

// MARK: - Messages from voxl's page

final class ScriptBridge: NSObject, WKScriptMessageHandler {
    static let clipboardHandler = "voxlClipboard"
    static let statusHandler = "voxlStatus"
    weak var owner: ViewerWindowController?

    /// voxl's page writes the remote clipboard with navigator.clipboard, which WebKit
    /// only allows during a user gesture. The shim hands those writes to the app, so a
    /// copy on the remote computer lands on this Mac's clipboard right away.
    static let clipboardShim = """
    (() => {
      const h = window.webkit?.messageHandlers?.\(clipboardHandler);
      if (!h) return;
      const b64 = (buf) => {
        const b = new Uint8Array(buf); let s = '';
        for (let i = 0; i < b.length; i += 0x8000) s += String.fromCharCode.apply(null, b.subarray(i, i + 0x8000));
        return btoa(s);
      };
      const clip = {
        writeText: async (t) => { h.postMessage({ text: String(t) }); },
        write: async (items) => {
          for (const it of items) {
            if (it.types.includes('image/png')) {
              h.postMessage({ png: b64(await (await it.getType('image/png')).arrayBuffer()) }); return;
            }
            if (it.types.includes('text/plain')) {
              h.postMessage({ text: await (await it.getType('text/plain')).text() }); return;
            }
          }
        },
        readText: async () => '',
        read: async () => [],
      };
      try { Object.defineProperty(navigator, 'clipboard', { value: clip, configurable: true }); } catch {}
    })();
    """

    /// voxl's page shows #status ("Connecting…", "Reconnecting…", or an error) until
    /// a stream starts, and hides it while frames arrive. Report its changes.
    static let statusWatcher = """
    (() => {
      const h = window.webkit?.messageHandlers?.\(statusHandler);
      if (!h) return;
      let last = '';
      const report = () => {
        const el = document.getElementById('status');
        if (!el) return;
        const shown = getComputedStyle(el).display !== 'none';
        const text = (el.textContent || '').trim();
        const state = !shown ? 'live'
          : /^Reconnecting/.test(text) ? 'reconnecting'
          : /^Connecting/.test(text) ? 'connecting' : 'error';
        if (state + text === last) return;
        last = state + text;
        h.postMessage({ state, text });
      };
      // voxl's toolbar is hidden (see nativeChrome); report what it offers instead.
      let lastTools = '';
      const tools = () => {
        const t = document.getElementById('tools');
        const picker = document.getElementById('display-picker');
        const menu = document.getElementById('display-menu');
        if (!t) return;
        const msg = {
          kind: 'tools',
          mic: t.classList.contains('mic'),
          micOn: t.classList.contains('mic-on'),
          files: t.classList.contains('files'),
          readOnly: t.classList.contains('read-only'),
          displays: picker && !picker.hidden && menu
            ? [...menu.querySelectorAll('.option')].map((o) => ({
                id: o.dataset.id || '', name: (o.textContent || '').trim(),
                selected: o.getAttribute('aria-selected') === 'true' }))
            : [],
        };
        const key = JSON.stringify(msg);
        if (key === lastTools) return;
        lastTools = key;
        h.postMessage(msg);
      };
      const start = () => {
        const el = document.getElementById('status');
        if (!el) return;
        new MutationObserver(report).observe(el, { attributes: true, childList: true, characterData: true, subtree: true });
        report();
        for (const id of ['tools', 'display-picker', 'display-menu']) {
          const n = document.getElementById(id);
          if (n) new MutationObserver(tools).observe(n, { attributes: true, childList: true, subtree: true });
        }
        tools();
      };
      if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', start);
      else start();
    })();
    """

    /// Hides voxl's own toolbar and status text: the window's toolbar and overlay
    /// take their place.
    static let nativeChrome = """
    (() => {
      const s = document.createElement('style');
      s.textContent = '#tools { display: none !important; } #status { opacity: 0 !important; }';
      document.head.appendChild(s);
    })();
    """

    /// The page's numbers, and the remote screen's shape (its surface is sized to it).
    static let sampleStats = """
    (() => {
      const s = window.voxlStats || {};
      const e = document.getElementById('surface');
      return JSON.stringify({
        fps: s.fps || 0, kbps: s.kbps || 0, decode: s.decodeMs || 0, pipe: s.pipeMs || 0,
        codec: s.codec || '',
        w: e ? parseFloat(e.style.width) || 0 : 0, h: e ? parseFloat(e.style.height) || 0 : 0,
      });
    })()
    """

    func userContentController(_ ucc: WKUserContentController, didReceive message: WKScriptMessage) {
        guard let owner, message.frameInfo.isMainFrame,
              let url = message.frameInfo.request.url, owner.isOwn(url),
              let body = message.body as? [String: Any] else { return }
        if message.name == Self.statusHandler {
            if body["kind"] as? String == "tools" {
                owner.pageTools(body)
            } else {
                owner.pageStatus(body["state"] as? String ?? "", body["text"] as? String ?? "")
            }
            return
        }
        let pb = NSPasteboard.general
        if let text = body["text"] as? String {
            pb.clearContents()
            pb.setString(text, forType: .string)
        } else if let png = body["png"] as? String, let data = Data(base64Encoded: png) {
            pb.clearContents()
            pb.setData(data, forType: .png)
        }
    }
}

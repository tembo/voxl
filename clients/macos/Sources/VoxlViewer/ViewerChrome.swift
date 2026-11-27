import AppKit
import Combine
import SwiftUI

/// Where a viewer's connection stands.
enum LinkState: Equatable {
    /// Loading the page or waiting for the first frame.
    case connecting
    /// Frames are arriving.
    case live
    /// The stream dropped; voxl's page is reconnecting on its own.
    case reconnecting
    /// The server isn't up; checking again every few seconds.
    case waiting(Health)
    /// Something that retrying won't fix.
    case failed(String)

    var subtitle: String {
        switch self {
        case .connecting: return "Connecting…"
        case .live: return "Connected"
        case .reconnecting: return "Reconnecting…"
        case .waiting(.notRunning): return "voxl isn't running"
        case .waiting(.untrusted): return "Certificate changed"
        case .waiting: return "Unreachable"
        case .failed: return "Not connected"
        }
    }
}

/// What voxl's page offers right now (its own toolbar is hidden; ours drives it).
struct PageTools: Equatable {
    struct Display: Equatable, Hashable { let id: String; let name: String; let selected: Bool }
    var mic = false
    var micOn = false
    var files = false
    var readOnly = false
    var displays: [Display] = []
}

/// The page's numbers, sampled once a second.
struct StreamStats: Equatable {
    var fps: Double = 0
    var kbps: Double = 0
    var decodeMs: Double = 0
    var pipeMs: Double = 0
    var codec = ""
}

/// Shared by a viewer window's overlay and toolbar.
final class ViewerModel: ObservableObject {
    @Published var name: String
    @Published var host: String
    @Published var state: LinkState = .connecting
    @Published var capturing = true
    @Published var appShortcuts = Prefs.captureAppShortcuts
    @Published var systemShortcutsWanted = Prefs.captureSystemShortcuts
    @Published var accessibility = KeyCapture.shared.accessibilityGranted
    @Published var nextTry: Date?
    @Published var tools = PageTools()
    @Published var stats = StreamStats()
    /// The last picture of this screen, shown blurred while connecting.
    @Published var backdrop: NSImage?

    var onClose: () -> Void = {}
    var onRetry: () -> Void = {}
    var onEdit: (() -> Void)?

    init(name: String, host: String) {
        self.name = name
        self.host = host
    }

    var needsAccessibility: Bool { appShortcuts && systemShortcutsWanted && !accessibility }
}

// MARK: - Overlay

/// Covers the stream whenever it isn't live, saying why and what happens next.
struct ViewerOverlay: View {
    @ObservedObject var model: ViewerModel

    var body: some View {
        ZStack {
            if model.state != .live {
                backdrop
                    .transition(.opacity)
                if model.state == .connecting {
                    VStack(spacing: 14) {
                        ProgressView().controlSize(.regular)
                        Text("Connecting to \(model.name)…")
                            .font(.system(size: 13, weight: .medium))
                            .foregroundStyle(.white.opacity(0.75))
                    }
                    .colorScheme(.dark)
                    .transition(.opacity)
                } else {
                    card
                        .transition(.scale(scale: 0.96).combined(with: .opacity))
                }
            }
        }
        .allowsHitTesting(model.state != .live)
        .animation(.spring(duration: 0.35), value: model.state)
    }

    /// The last picture of the screen, softly blurred, or a dark gradient.
    @ViewBuilder private var backdrop: some View {
        ZStack {
            Color.black
            if let img = model.backdrop {
                Image(nsImage: img)
                    .resizable()
                    .aspectRatio(contentMode: .fill)
                    .blur(radius: 28, opaque: true)
                    .saturation(0.8)
                    .overlay(Color.black.opacity(0.45))
                    .clipped()
                    .transition(.opacity)
            } else {
                LinearGradient(colors: [Color(white: 0.12), Color(white: 0.04)], startPoint: .top, endPoint: .bottom)
            }
        }
        .ignoresSafeArea()
    }

    private var card: some View {
        VStack(spacing: 0) {
            ZStack {
                Circle().fill(.white.opacity(0.08)).frame(width: 64, height: 64)
                Image(systemName: symbol)
                    .font(.system(size: 26, weight: .regular))
                    .foregroundStyle(.white.opacity(0.9))
                    .symbolEffect(.pulse, isActive: model.state == .reconnecting)
            }
            .padding(.bottom, 18)
            Text(title)
                .font(.system(size: 17, weight: .semibold))
                .multilineTextAlignment(.center)
            Text(detail)
                .font(.system(size: 13))
                .foregroundStyle(.white.opacity(0.65))
                .multilineTextAlignment(.center)
                .lineSpacing(2)
                .fixedSize(horizontal: false, vertical: true)
                .padding(.top, 6)
            footer.padding(.top, 22)
        }
        .foregroundStyle(.white)
        .padding(.horizontal, 32).padding(.vertical, 28)
        .frame(width: 400)
        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 22, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 22, style: .continuous).strokeBorder(.white.opacity(0.1)))
        .shadow(color: .black.opacity(0.35), radius: 30, y: 12)
        .colorScheme(.dark)
    }

    @ViewBuilder private var footer: some View {
        VStack(spacing: 14) {
            HStack(spacing: 10) {
                Button { model.onClose() } label: { Text("Close").frame(minWidth: 70) }
                if case .waiting(.untrusted) = model.state, let edit = model.onEdit {
                    Button { edit() } label: { Text("Edit…").frame(minWidth: 70) }
                }
                Button { model.onRetry() } label: { Text(retrying ? "Try Now" : "Try Again").frame(minWidth: 70) }
                    .keyboardShortcut(.defaultAction)
            }
            .controlSize(.large)
            if retrying {
                HStack(spacing: 6) {
                    ProgressView().controlSize(.mini)
                    Text("Checking every few seconds")
                }
                .font(.system(size: 11))
                .foregroundStyle(.white.opacity(0.5))
            }
        }
    }

    private var retrying: Bool {
        if case .failed = model.state { return false }
        return true
    }

    private var symbol: String {
        switch model.state {
        case .reconnecting: return "arrow.triangle.2.circlepath"
        case .waiting(.notRunning): return "pause.fill"
        case .waiting(.untrusted): return "lock.trianglebadge.exclamationmark"
        case .waiting: return "moon.zzz.fill"
        case .failed: return "exclamationmark.triangle.fill"
        default: return "display"
        }
    }

    private var title: String {
        switch model.state {
        case .reconnecting: return "Reconnecting to \(model.name)"
        case .waiting(.notRunning): return "voxl isn't running on \(model.name)"
        case .waiting(.untrusted): return "\(model.name)'s certificate changed"
        case .waiting: return "Can't reach \(model.name)"
        case .failed: return "Couldn't connect to \(model.name)"
        default: return "Connecting to \(model.name)"
        }
    }

    private var detail: String {
        switch model.state {
        case .reconnecting:
            return "The stream stopped. It picks up again on its own as soon as \(model.name) is back."
        case .waiting(.notRunning):
            return "\(model.name) is online, but voxl isn't answering. Start voxl there and this window connects on its own."
        case .waiting(.unreachable(let why)):
            return why + " This window connects on its own once it's back."
        case .waiting(.untrusted):
            return "It no longer matches the certificate you trusted for this computer."
        case .failed(let why):
            return why
        default:
            return ""
        }
    }
}

// MARK: - Connection badge

/// A small live readout in the toolbar; click for the details.
struct QualityBadge: View {
    @ObservedObject var model: ViewerModel
    @ObservedObject var popover: PopoverToggle

    var body: some View {
        Button { popover.shown.toggle() } label: {
            HStack(spacing: 6) {
                Circle().fill(color).frame(width: 7, height: 7)
                    .shadow(color: color.opacity(0.6), radius: 3)
                Text(label)
                    .font(.system(size: 11, weight: .medium).monospacedDigit())
                    .foregroundStyle(.secondary)
            }
            .padding(.horizontal, 10)
            .frame(height: 24)
            .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .help("Connection details")
        .popover(isPresented: $popover.shown, arrowEdge: .bottom) { details }
    }

    private var color: Color {
        switch model.state {
        case .live: return .green
        case .connecting, .reconnecting: return .yellow
        default: return .red
        }
    }

    private var label: String {
        guard model.state == .live else { return model.state.subtitle }
        let fps = Int(model.stats.fps.rounded())
        return fps > 0 ? "\(fps) fps" : "Live"
    }

    private var details: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Text(model.name).font(.headline)
                Spacer()
                Text(model.state.subtitle).font(.caption).foregroundStyle(.secondary)
            }
            Divider()
            Grid(alignment: .leading, horizontalSpacing: 16, verticalSpacing: 6) {
                row("Frame rate", model.stats.fps > 0.5 ? String(format: "%.0f fps", model.stats.fps) : "Idle — the screen is still")
                row("Bandwidth", String(format: "%.1f Mbps", model.stats.kbps / 1000))
                row("Decode", String(format: "%.1f ms", model.stats.decodeMs))
                row("Server encode", String(format: "%.1f ms", model.stats.pipeMs))
                row("Codec", model.stats.codec.isEmpty ? "—" : "H.264 (\(model.stats.codec))")
                row("Address", model.host)
            }
            .font(.system(size: 12))
            Text("voxl sends frames only when something on screen changes, so a still screen uses almost nothing.")
                .font(.caption)
                .foregroundStyle(.tertiary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(16)
        .frame(width: 300)
    }

    private func row(_ k: String, _ v: String) -> some View {
        GridRow {
            Text(k).foregroundStyle(.secondary)
            Text(v).monospacedDigit().textSelection(.enabled)
        }
    }
}

/// Holds a popover's open state (no @State: see ConnectionsModel).
final class PopoverToggle: ObservableObject {
    @Published var shown = false
}

/// Asks for Accessibility so system shortcuts reach the remote computer.
struct AllowShortcutsButton: View {
    @ObservedObject var model: ViewerModel

    var body: some View {
        if model.needsAccessibility && model.capturing {
            Button {
                KeyCapture.shared.requestAccessibility()
                NSWorkspace.shared.open(URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")!)
            } label: {
                HStack(spacing: 5) {
                    Image(systemName: "exclamationmark.lock.fill")
                    Text("Allow ⌘Tab & Raycast")
                }
                .font(.system(size: 11, weight: .semibold))
                .foregroundStyle(.orange)
                .padding(.horizontal, 10)
                .frame(height: 24)
                .background(.orange.opacity(0.14), in: Capsule())
            }
            .buttonStyle(.plain)
            .help("System shortcuts like ⌘Tab, ⌘Space and Raycast's hotkey stay on this Mac until Voxl Viewer has Accessibility permission. Click to allow it in System Settings.")
        }
    }
}

// MARK: - Toolbar

/// The viewer's native toolbar. It replaces voxl's in-page toolbar (hidden by the
/// viewer) and drives the same controls.
final class ViewerToolbar: NSObject, NSToolbarDelegate {
    private enum ID {
        static let allow = NSToolbarItem.Identifier("allow")
        static let quality = NSToolbarItem.Identifier("quality")
        static let displays = NSToolbarItem.Identifier("displays")
        static let mic = NSToolbarItem.Identifier("mic")
        static let files = NSToolbarItem.Identifier("files")
        static let keyboard = NSToolbarItem.Identifier("keyboard")
        static let fullScreen = NSToolbarItem.Identifier("fullScreen")
    }

    let toolbar = NSToolbar(identifier: "Viewer")
    private let model: ViewerModel
    private let popover = PopoverToggle()
    private var items: [NSToolbarItem.Identifier: NSToolbarItem] = [:]
    private var bag: AnyCancellable?

    var onMic: () -> Void = {}
    var onFiles: () -> Void = {}
    var onDisplay: (String) -> Void = { _ in }
    var onToggleCapture: () -> Void = {}
    var onFullScreen: () -> Void = {}

    init(model: ViewerModel) {
        self.model = model
        super.init()
        toolbar.delegate = self
        toolbar.displayMode = .iconOnly
        toolbar.allowsUserCustomization = false
        bag = model.objectWillChange
            .receive(on: RunLoop.main)
            .sink { [weak self] _ in DispatchQueue.main.async { self?.update() } }
    }

    func toolbarDefaultItemIdentifiers(_ toolbar: NSToolbar) -> [NSToolbarItem.Identifier] {
        [.flexibleSpace, ID.allow, ID.quality, .space, ID.displays, ID.mic, ID.files, ID.keyboard, ID.fullScreen]
    }

    func toolbarAllowedItemIdentifiers(_ toolbar: NSToolbar) -> [NSToolbarItem.Identifier] {
        toolbarDefaultItemIdentifiers(toolbar)
    }

    func toolbar(_ toolbar: NSToolbar, itemForItemIdentifier id: NSToolbarItem.Identifier,
                 willBeInsertedIntoToolbar flag: Bool) -> NSToolbarItem? {
        let item: NSToolbarItem
        switch id {
        case ID.allow:
            item = NSToolbarItem(itemIdentifier: id)
            let v = NSHostingView(rootView: AllowShortcutsButton(model: model))
            v.frame.size = NSSize(width: 170, height: 28)
            item.view = v
            item.label = "Allow Shortcuts"
        case ID.quality:
            item = NSToolbarItem(itemIdentifier: id)
            let v = NSHostingView(rootView: QualityBadge(model: model, popover: popover))
            v.frame.size = NSSize(width: 104, height: 28)
            item.view = v
            item.label = "Connection"
        case ID.displays:
            let m = NSMenuToolbarItem(itemIdentifier: id)
            m.image = NSImage(systemSymbolName: "rectangle.on.rectangle", accessibilityDescription: "Display")
            m.label = "Display"
            m.toolTip = "Choose which display to show"
            m.showsIndicator = true
            m.menu = NSMenu()
            item = m
        case ID.mic:
            item = button(id, "mic.slash", "Microphone", #selector(mic))
        case ID.files:
            item = button(id, "folder", "Files", #selector(files))
            item.toolTip = "Browse and transfer files"
        case ID.keyboard:
            item = button(id, "command", "Keyboard", #selector(keyboard))
        case ID.fullScreen:
            item = button(id, "arrow.up.left.and.arrow.down.right", "Full Screen", #selector(fullScreen))
            item.toolTip = "Enter full screen (⌃⌘F)"
        default:
            return nil
        }
        items[id] = item
        update()
        return item
    }

    private func button(_ id: NSToolbarItem.Identifier, _ symbol: String, _ label: String, _ action: Selector) -> NSToolbarItem {
        let item = NSToolbarItem(itemIdentifier: id)
        item.image = NSImage(systemSymbolName: symbol, accessibilityDescription: label)
        item.label = label
        item.isBordered = true
        item.target = self
        item.action = action
        return item
    }

    @objc private func mic() { onMic() }
    @objc private func files() { onFiles() }
    @objc private func keyboard() { onToggleCapture() }
    @objc private func fullScreen() { onFullScreen() }
    @objc private func pickDisplay(_ sender: NSMenuItem) {
        if let id = sender.representedObject as? String { onDisplay(id) }
    }

    /// Brings the items in line with the page and the connection.
    func update() {
        let live = model.state == .live
        let t = model.tools

        if let mic = items[ID.mic] {
            mic.image = NSImage(systemSymbolName: t.micOn ? "mic.fill" : "mic.slash",
                                accessibilityDescription: "Microphone")
            mic.toolTip = t.micOn ? "Turn microphone off" : "Send your microphone to \(model.name)"
            mic.isEnabled = live && t.mic
            hide(mic, !t.mic)
        }
        if let files = items[ID.files] {
            files.isEnabled = live && t.files
            hide(files, !t.files)
        }
        if let kb = items[ID.keyboard] {
            let on = model.capturing && model.appShortcuts
            kb.image = NSImage(systemSymbolName: on ? "command.square.fill" : "command.square",
                               accessibilityDescription: "Keyboard")
            kb.toolTip = on
                ? "⌘ shortcuts go to \(model.name). Click or press ⌃⌥K to use them on this Mac."
                : "⌘ shortcuts work on this Mac. Click or press ⌃⌥K to send them to \(model.name)."
            hide(kb, !model.appShortcuts || t.readOnly)
        }
        if let d = items[ID.displays] as? NSMenuToolbarItem {
            let menu = NSMenu()
            for disp in t.displays {
                let mi = NSMenuItem(title: disp.name, action: #selector(pickDisplay(_:)), keyEquivalent: "")
                mi.target = self
                mi.representedObject = disp.id
                mi.state = disp.selected ? .on : .off
                menu.addItem(mi)
            }
            d.menu = menu
            d.isEnabled = live
            hide(d, t.displays.count < 2 || t.readOnly)
        }
        if let allow = items[ID.allow] { hide(allow, !(model.needsAccessibility && model.capturing)) }
    }

    private func hide(_ item: NSToolbarItem, _ hidden: Bool) {
        if #available(macOS 15.0, *) { item.isHidden = hidden }
    }
}

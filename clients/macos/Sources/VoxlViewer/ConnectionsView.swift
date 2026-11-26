import AppKit
import Combine
import SwiftUI

// The main window: a sidebar (Computers, Tailnet) and a gallery of screens, after
// macOS's Screen Sharing. No @State anywhere: the app builds without Xcode, whose
// macro plugins SwiftUI's @State needs, so view state lives in small model objects.

extension Notification.Name {
    static let addComputer = Notification.Name("VoxlViewer.addComputer")
}

enum SidebarSection: String, Hashable { case computers, tailnet }

/// What the editor sheet is editing.
struct EditorItem: Identifiable {
    let id = UUID()
    var connection: Connection
    var password: String
    var isNew: Bool
    /// Open a viewer once it's saved ("Connect" rather than "Add").
    var connectAfter = false
    /// Coming from the tailnet: only the password is missing.
    var passwordOnly = false
    var symbol = "display"
}

final class GalleryModel: ObservableObject {
    @Published var section: SidebarSection? = .computers
    @Published var search = ""
    @Published var selection: String?
    @Published var editing: EditorItem?
    @Published var setupFor: TailnetDevice?
}

/// Whether each saved computer's voxl is up, checked while the window is open.
@MainActor
final class SavedStatus: ObservableObject {
    @Published private(set) var health: [UUID: Health] = [:]

    func refresh(_ connections: [Connection]) async {
        await withTaskGroup(of: (UUID, Health).self) { group in
            for c in connections {
                guard let base = c.baseURL else { continue }
                group.addTask { (c.id, await Health.check(base, pinned: c.certFingerprint, timeout: 3)) }
            }
            for await (id, h) in group { health[id] = h }
        }
    }
}

struct ConnectionsView: View {
    @EnvironmentObject var store: ConnectionStore
    @StateObject private var discovery = TailnetDiscovery()
    @StateObject private var saved = SavedStatus()
    @StateObject private var model = GalleryModel()
    @ObservedObject private var thumbs = Thumbnails.shared
    @AppStorage("galleryLayout") private var layout = "grid"
    private let ticker = Timer.publish(every: 20, on: .main, in: .common).autoconnect()
    private let activated = NotificationCenter.default.publisher(for: NSApplication.didBecomeActiveNotification)
    private let addRequested = NotificationCenter.default.publisher(for: .addComputer)

    var body: some View {
        NavigationSplitView {
            sidebar
        } detail: {
            detail
                .navigationTitle(model.section == .tailnet ? "Tailnet" : "Computers")
                .navigationSubtitle(subtitle)
        }
        .searchable(text: $model.search, placement: .toolbar, prompt: "Search")
        .toolbar {
            ToolbarItemGroup(placement: .primaryAction) {
                Picker("View", selection: $layout) {
                    Label("Gallery", systemImage: "square.grid.2x2").tag("grid")
                    Label("List", systemImage: "list.bullet").tag("list")
                }
                .pickerStyle(.segmented)
                .help("Show as gallery or list")
                Button { Task { await refreshAll(force: true) } } label: {
                    Label("Refresh", systemImage: "arrow.clockwise")
                }
                .help("Check every computer again")
                Button { model.editing = newItem() } label: { Label("Add Computer", systemImage: "plus") }
                    .help("Add a computer by address or sign-in link")
            }
        }
        .frame(minWidth: 680, minHeight: 440)
        .sheet(item: $model.editing) { item in
            ConnectionEditor(item: item, thumbnail: thumbs.images[item.connection.id]) { edited, password in
                // Adding an address that's already saved updates that computer instead.
                var conn = edited
                if item.isNew, let same = store.connections.first(where: {
                    $0.baseURL?.host == conn.baseURL?.host && ($0.baseURL?.port ?? 443) == (conn.baseURL?.port ?? 443)
                }) {
                    conn.id = same.id
                    conn.certFingerprint = conn.certFingerprint ?? same.certFingerprint
                }
                store.upsert(conn)
                if !conn.storePassword(password) {
                    let a = NSAlert()
                    a.messageText = "Couldn't save the password"
                    a.informativeText = "macOS didn't let Voxl Viewer store it in your Keychain. It's used for this connection only."
                    a.runModal()
                }
                ViewerWindowController.update(conn, password: password)
                model.editing = nil
                model.section = .computers
                model.selection = conn.id.uuidString
                if item.connectAfter { ViewerWindowController.show(conn, password: password) }
                Task {
                    await saved.refresh(store.connections)
                    await thumbs.refresh(conn, force: true)
                }
            } cancel: { model.editing = nil }
        }
        .sheet(item: $model.setupFor) { d in
            SetupSheet(device: d) {
                model.setupFor = nil
                Task { await discovery.refresh() }
            }
        }
        .task { await refreshAll() }
        .onReceive(ticker) { _ in Task { await refreshAll() } }
        .onReceive(activated) { _ in Task { await refreshAll() } }
        .onReceive(addRequested) { _ in model.editing = newItem() }
    }

    private var subtitle: String {
        switch model.section {
        case .tailnet?:
            let ready = discovery.devices.filter { $0.voxl.isReady }.count
            return discovery.devices.isEmpty ? "" : "\(ready) of \(discovery.devices.count) running voxl"
        default:
            let up = store.connections.filter { saved.health[$0.id] == .up }.count
            return store.connections.isEmpty ? "" : "\(up) of \(store.connections.count) ready"
        }
    }

    private func refreshAll(force: Bool = false) async {
        async let a: Void = discovery.refresh()
        async let b: Void = saved.refresh(store.connections)
        _ = await (a, b)
        for c in store.connections where saved.health[c.id] == .up {
            await thumbs.refresh(c, force: force)
        }
    }

    // MARK: Sidebar

    private var sidebar: some View {
        List(selection: $model.section) {
            Label("Computers", systemImage: "display")
                .badge(store.connections.count)
                .tag(SidebarSection.computers)
            Label("Tailnet", systemImage: "network")
                .badge(discovery.devices.filter { $0.voxl.isReady }.count)
                .tag(SidebarSection.tailnet)
        }
        .navigationSplitViewColumnWidth(min: 170, ideal: 190, max: 260)
    }

    // MARK: Detail

    @ViewBuilder private var detail: some View {
        switch model.section {
        case .tailnet?: tailnet
        default: computers
        }
    }

    private func matches(_ s: String...) -> Bool {
        let q = model.search.trimmingCharacters(in: .whitespaces)
        return q.isEmpty || s.contains { $0.localizedCaseInsensitiveContains(q) }
    }

    @ViewBuilder private var computers: some View {
        let list = store.connections.filter { matches($0.name, $0.host) }
        if store.connections.isEmpty {
            ContentUnavailableView {
                Label("No Computers Yet", systemImage: "display")
            } description: {
                Text("Connect to a computer on your tailnet, or add one by its address or the link voxl prints when it starts.")
            } actions: {
                HStack {
                    Button("Add Computer…") { model.editing = newItem() }
                    Button("Browse Tailnet") { model.section = .tailnet }
                        .buttonStyle(.borderedProminent)
                }
            }
        } else if list.isEmpty {
            ContentUnavailableView.search(text: model.search)
        } else {
            collection(list.map { GalleryItem.saved($0) })
        }
    }

    @ViewBuilder private var tailnet: some View {
        switch discovery.state {
        case .unavailable(let why, let action):
            ContentUnavailableView {
                Label("Tailnet Unavailable", systemImage: "network.slash")
            } description: {
                Text(why)
            } actions: {
                switch action {
                case .openTailscale?: Button("Open Tailscale") { openTailscale() }.buttonStyle(.borderedProminent)
                case .installTailscale?:
                    Button("Get Tailscale") { NSWorkspace.shared.open(URL(string: "https://tailscale.com/download/mac")!) }
                        .buttonStyle(.borderedProminent)
                case nil: EmptyView()
                }
            }
        case .idle, .searching where discovery.devices.isEmpty:
            VStack(spacing: 12) {
                ProgressView()
                Text("Looking for computers on your tailnet…").foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        default:
            let list = discovery.devices.filter { matches($0.name, $0.dns) }
            if discovery.devices.isEmpty {
                ContentUnavailableView("No Computers Online",
                                       systemImage: "network",
                                       description: Text("Other computers on your tailnet show up here when they're online."))
            } else if list.isEmpty {
                ContentUnavailableView.search(text: model.search)
            } else {
                collection(list.map { d in GalleryItem.device(d, saved: savedFor(d)) },
                           footer: discovery.offlineCount > 0 ? "\(discovery.offlineCount) more offline" : nil)
            }
        }
    }

    private func savedFor(_ d: TailnetDevice) -> Connection? {
        if case .ready(let url, _) = d.voxl, let u = Connection.normalize(url) {
            return store.connections.first { $0.baseURL?.host == u.host && ($0.baseURL?.port ?? 443) == (u.port ?? 443) }
        }
        return store.connections.first { $0.host == d.dns }
    }

    @ViewBuilder private func collection(_ items: [GalleryItem], footer: String? = nil) -> some View {
        ScrollView {
            if layout == "list" {
                LazyVStack(spacing: 0) {
                    ForEach(items) { item in
                        ComputerRow(info: info(item), selected: model.selection == item.id)
                            .onTapGesture(count: 2) { primary(item) }
                            .simultaneousGesture(TapGesture().onEnded { model.selection = item.id })
                            .contextMenu { menu(item) }
                        if item.id != items.last?.id { Divider().padding(.leading, 76) }
                    }
                }
                .padding(.horizontal, 16).padding(.vertical, 10)
            } else {
                LazyVGrid(columns: [GridItem(.adaptive(minimum: 230, maximum: 340), spacing: 22)], spacing: 26) {
                    ForEach(items) { item in
                        ComputerCard(info: info(item), selected: model.selection == item.id) { primary(item) }
                            .onTapGesture(count: 2) { primary(item) }
                            .simultaneousGesture(TapGesture().onEnded { model.selection = item.id })
                            .contextMenu { menu(item) }
                    }
                }
                .padding(24)
            }
            if let footer {
                Text(footer).font(.callout).foregroundStyle(.tertiary).padding(.bottom, 20)
            }
        }
        .background(Color(nsColor: .windowBackgroundColor))
        .onTapGesture { model.selection = nil }
    }

    // MARK: Items

    private func info(_ item: GalleryItem) -> CardInfo {
        switch item {
        case .saved(let c):
            return savedInfo(c, symbol: "display")
        case .device(let d, let s?):
            var i = savedInfo(s, symbol: d.symbol)
            i.badge = d.isSelf ? "This Mac" : nil
            return i
        case .device(let d, nil):
            let (color, text): (Color?, String) = {
                switch d.voxl {
                case .checking: return (nil, "Checking…")
                case .ready(_, let selfSigned): return (.green, selfSigned ? "Ready · self-signed" : "Ready to connect")
                case .httpOnly: return (.orange, "voxl needs HTTPS")
                case .notRunning: return (Color(white: 0.55), "voxl isn't running")
                }
            }()
            return CardInfo(title: d.name, subtitle: d.dns, symbol: d.symbol, image: nil,
                            statusColor: color, statusText: text, badge: d.isSelf ? "This Mac" : nil,
                            action: d.voxl.isReady ? "Connect" : (d.voxl == .checking ? nil : "Set Up"),
                            dimmed: !d.voxl.isReady)
        }
    }

    private func savedInfo(_ c: Connection, symbol: String) -> CardInfo {
        let host = c.baseURL.map { ($0.host ?? "") + ($0.port.map { ":\($0)" } ?? "") } ?? c.url
        let (color, text): (Color?, String) = {
            if !c.isSecureContext { return (.orange, "Needs HTTPS") }
            switch saved.health[c.id] {
            case nil: return (nil, "Checking…")
            case .up?: return (.green, "Ready")
            case .notRunning?: return (.orange, "voxl isn't running")
            case .untrusted?: return (.red, "Certificate changed")
            case .unreachable?: return (Color(white: 0.55), "Offline")
            }
        }()
        return CardInfo(title: c.name, subtitle: host, symbol: symbol, image: thumbs.images[c.id],
                        statusColor: color, statusText: text, badge: nil, action: "Connect",
                        dimmed: saved.health[c.id].map { !$0.isUp } ?? false)
    }

    private func primary(_ item: GalleryItem) {
        switch item {
        case .saved(let c), .device(_, let c?):
            ViewerWindowController.show(c)
        case .device(let d, nil):
            if case .ready(let url, _) = d.voxl {
                model.editing = EditorItem(connection: Connection(name: d.name, url: url), password: "",
                                           isNew: true, connectAfter: true, passwordOnly: true, symbol: d.symbol)
            } else if d.voxl != .checking {
                model.setupFor = d
            }
        }
    }

    @ViewBuilder private func menu(_ item: GalleryItem) -> some View {
        switch item {
        case .saved(let c), .device(_, let c?):
            Button("Connect") { ViewerWindowController.show(c) }
            Button("Edit…") { model.editing = EditorItem(connection: c, password: c.password ?? "", isNew: false) }
            Divider()
            Button("Open in Browser") { if let u = c.loadURL(password: c.password) { NSWorkspace.shared.open(u) } }
            Button("Copy Sign-in Link") {
                if let u = c.loadURL(password: c.password) {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(u.absoluteString, forType: .string)
                }
            }
            Divider()
            Button("Remove", role: .destructive) {
                store.delete(c)
                thumbs.remove(c.id)
            }
        case .device(let d, nil):
            Button(d.voxl.isReady ? "Connect…" : "Set Up…") { primary(item) }
            Button("Copy Address") {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(d.dns, forType: .string)
            }
        }
    }

    private func openTailscale() {
        if let app = NSWorkspace.shared.urlForApplication(withBundleIdentifier: "io.tailscale.ipn.macos")
            ?? NSWorkspace.shared.urlForApplication(withBundleIdentifier: "io.tailscale.ipn.macsys") {
            NSWorkspace.shared.openApplication(at: app, configuration: .init())
        }
    }

    private func newItem() -> EditorItem {
        // A copied sign-in link fills in the form.
        var link = ""
        if let s = NSPasteboard.general.string(forType: .string), s.contains("token="),
           Connection.normalize(s) != nil { link = s }
        let (url, token) = Connection.split(link: link)
        return EditorItem(connection: Connection(name: "", url: link.isEmpty ? "" : url),
                          password: token ?? "", isNew: true, connectAfter: true)
    }
}

// MARK: - Cards

enum GalleryItem: Identifiable {
    case saved(Connection)
    case device(TailnetDevice, saved: Connection?)

    var id: String {
        switch self {
        case .saved(let c): return c.id.uuidString
        case .device(let d, _): return d.dns
        }
    }
}

struct CardInfo {
    var title: String
    var subtitle: String
    var symbol: String
    var image: NSImage?
    var statusColor: Color?
    var statusText: String
    var badge: String?
    /// The hover button: "Connect", "Set Up", or none.
    var action: String?
    var dimmed: Bool
}

final class HoverState: ObservableObject {
    @Published var on = false
}

/// A screen in the gallery: its last picture (or a placeholder), name and status.
struct ComputerCard: View {
    let info: CardInfo
    let selected: Bool
    let open: () -> Void
    @StateObject private var hover = HoverState()

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            // The card's shape comes from the clear rectangle; the picture fills it
            // without pushing the layout around.
            Color.clear
                .aspectRatio(16 / 10, contentMode: .fit)
                .overlay { ZStack {
                Screen(info: info)
                if hover.on, let action = info.action {
                    Color.black.opacity(0.28)
                    Button(action: open) {
                        Text(action)
                            .font(.system(size: 13, weight: .semibold))
                            .padding(.horizontal, 18).padding(.vertical, 8)
                            .background(.ultraThinMaterial, in: Capsule())
                            .overlay(Capsule().strokeBorder(.white.opacity(0.2)))
                            .colorScheme(.dark)
                    }
                    .buttonStyle(.plain)
                    .transition(.opacity.combined(with: .scale(scale: 0.92)))
                }
            } }
            .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
            .overlay(
                RoundedRectangle(cornerRadius: 12, style: .continuous)
                    .strokeBorder(selected ? Color.accentColor : Color.primary.opacity(0.1),
                                  lineWidth: selected ? 3 : 1)
            )
            .shadow(color: .black.opacity(hover.on ? 0.28 : 0.14), radius: hover.on ? 14 : 6, y: hover.on ? 8 : 3)
            .scaleEffect(hover.on ? 1.012 : 1)

            HStack(alignment: .firstTextBaseline, spacing: 6) {
                VStack(alignment: .leading, spacing: 3) {
                    HStack(spacing: 6) {
                        Text(info.title).font(.system(size: 13, weight: .semibold)).lineLimit(1)
                        if let badge = info.badge { Badge(text: badge) }
                    }
                    StatusLine(color: info.statusColor, text: info.statusText)
                }
                Spacer(minLength: 0)
            }
            .padding(.horizontal, 2)
        }
        .contentShape(Rectangle())
        .onHover { h in withAnimation(.easeOut(duration: 0.15)) { hover.on = h } }
    }
}

/// The picture area of a card or row.
private struct Screen: View {
    let info: CardInfo
    var compact = false

    var body: some View {
        ZStack {
            if let img = info.image {
                Color.black.overlay {
                    Image(nsImage: img)
                        .resizable()
                        .interpolation(.high)
                        .aspectRatio(contentMode: .fill)
                        .saturation(info.dimmed ? 0 : 1)
                        .opacity(info.dimmed ? 0.55 : 1)
                }
            } else {
                Placeholder(seed: info.title, symbol: info.symbol, compact: compact)
                    .opacity(info.dimmed ? 0.6 : 1)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .clipped()
    }
}

/// A calm gradient for computers without a picture yet, tinted per name.
private struct Placeholder: View {
    let seed: String
    let symbol: String
    var compact = false

    private var hue: Double {
        Double(seed.unicodeScalars.reduce(UInt32(7)) { ($0 &* 31) &+ $1.value } % 360) / 360
    }

    var body: some View {
        ZStack {
            LinearGradient(colors: [Color(hue: hue, saturation: 0.42, brightness: 0.42),
                                    Color(hue: (hue + 0.07).truncatingRemainder(dividingBy: 1), saturation: 0.55, brightness: 0.2)],
                           startPoint: .topLeading, endPoint: .bottomTrailing)
            RadialGradient(colors: [.white.opacity(0.14), .clear], center: .topLeading, startRadius: 0, endRadius: 260)
            Image(systemName: symbol)
                .font(.system(size: compact ? 18 : 44, weight: .thin))
                .foregroundStyle(.white.opacity(0.7))
        }
    }
}

private struct Badge: View {
    let text: String
    var body: some View {
        Text(text)
            .font(.system(size: 10, weight: .medium))
            .padding(.horizontal, 6).padding(.vertical, 1.5)
            .background(.quaternary, in: Capsule())
            .foregroundStyle(.secondary)
    }
}

/// A coloured dot and a few words; a spinner while checking.
struct StatusLine: View {
    let color: Color?
    let text: String
    var body: some View {
        HStack(spacing: 5) {
            if let color {
                Circle().fill(color).frame(width: 6, height: 6)
            } else {
                ProgressView().controlSize(.mini).scaleEffect(0.7).frame(width: 8, height: 8)
            }
            Text(text).lineLimit(1)
        }
        .font(.system(size: 11))
        .foregroundStyle(.secondary)
    }
}

/// A computer in list layout.
struct ComputerRow: View {
    let info: CardInfo
    let selected: Bool
    @StateObject private var hover = HoverState()

    var body: some View {
        HStack(spacing: 14) {
            Screen(info: info, compact: true)
                .frame(width: 64, height: 40)
                .clipShape(RoundedRectangle(cornerRadius: 6, style: .continuous))
                .overlay(RoundedRectangle(cornerRadius: 6, style: .continuous).strokeBorder(.primary.opacity(0.1)))
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 6) {
                    Text(info.title).font(.system(size: 13, weight: .semibold))
                    if let badge = info.badge { Badge(text: badge) }
                }
                Text(info.subtitle).font(.system(size: 11)).foregroundStyle(.tertiary).lineLimit(1)
            }
            Spacer()
            StatusLine(color: info.statusColor, text: info.statusText)
                .frame(width: 150, alignment: .leading)
        }
        .padding(.horizontal, 10).padding(.vertical, 8)
        .background(
            RoundedRectangle(cornerRadius: 8, style: .continuous)
                .fill(selected ? Color.accentColor.opacity(0.18) : (hover.on ? Color.primary.opacity(0.05) : .clear))
        )
        .contentShape(Rectangle())
        .onHover { h in hover.on = h }
    }
}

// MARK: - Setting up a computer

/// How to get voxl running on a tailnet computer so this Mac can view it.
struct SetupSheet: View {
    let device: TailnetDevice
    let done: () -> Void

    private var commands: String {
        let dns = device.dns
        let install = device.os.lowercased() == "macos"
            ? "brew install x264 opus pkgconf esbuild\ncargo install voxl-server\n"
            : "cargo install voxl-server\n"
        return install + """
        voxl --port=6901 --allowed-hosts=\(dns) &
        tailscale serve --bg --https=6443 http://127.0.0.1:6901
        """
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            SheetHeader(symbol: device.symbol, image: nil, title: "Set Up \(device.name)", subtitle: device.dns)
            Text(device.voxl == .httpOnly
                 ? "voxl is running on \(device.name), but only over plain HTTP, and video only plays over HTTPS. Serve it through Tailscale:"
                 : "Install voxl on \(device.name) and serve it over HTTPS on your tailnet. In Terminal on that computer:")
                .fixedSize(horizontal: false, vertical: true)
            CodeBlock(text: device.voxl == .httpOnly
                      ? "tailscale serve --bg --https=6443 http://127.0.0.1:6901"
                      : commands)
            Label("macOS asks that computer for Screen Recording and Accessibility the first time voxl runs. \(device.name) then shows up here as ready.",
                  systemImage: "info.circle")
                .font(.callout).foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            HStack {
                Link("About voxl", destination: URL(string: "https://github.com/tembo/voxl")!).font(.callout)
                Spacer()
                Button("Done", action: done).keyboardShortcut(.defaultAction).controlSize(.large)
            }
        }
        .padding(24)
        .frame(width: 540)
    }
}

private struct CodeBlock: View {
    let text: String
    @StateObject private var copied = HoverState()

    var body: some View {
        HStack(alignment: .top) {
            Text(text)
                .font(.system(size: 12, design: .monospaced))
                .lineSpacing(3)
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
            Button {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(text, forType: .string)
                copied.on = true
                DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) { copied.on = false }
            } label: {
                Image(systemName: copied.on ? "checkmark" : "doc.on.doc")
                    .contentTransition(.symbolEffect(.replace))
            }
            .buttonStyle(.borderless)
            .help("Copy")
        }
        .padding(12)
        .background(Color.primary.opacity(0.05), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 10, style: .continuous).strokeBorder(.primary.opacity(0.08)))
    }
}

/// An icon tile, a title and a subtitle, at the top of a sheet.
private struct SheetHeader: View {
    let symbol: String
    let image: NSImage?
    let title: String
    let subtitle: String

    var body: some View {
        HStack(spacing: 14) {
            ZStack {
                if let image {
                    Image(nsImage: image).resizable().aspectRatio(contentMode: .fill)
                } else {
                    Placeholder(seed: title, symbol: symbol, compact: true)
                }
            }
            .frame(width: 64, height: 42)
            .clipShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
            .overlay(RoundedRectangle(cornerRadius: 8, style: .continuous).strokeBorder(.primary.opacity(0.1)))
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(.system(size: 15, weight: .semibold))
                if !subtitle.isEmpty {
                    Text(subtitle).font(.system(size: 12)).foregroundStyle(.secondary).lineLimit(1)
                }
            }
        }
    }
}

// MARK: - Editor

final class EditorModel: ObservableObject {
    @Published var name: String
    @Published var url: String
    @Published var password: String
    init(_ item: EditorItem) {
        name = item.connection.name
        url = item.connection.url
        password = item.password
    }
}

/// Add, edit, or (from the tailnet) just enter the password for a computer.
struct ConnectionEditor: View {
    @StateObject private var m: EditorModel
    private let item: EditorItem
    private let thumbnail: NSImage?
    let save: (Connection, String) -> Void
    let cancel: () -> Void

    init(item: EditorItem, thumbnail: NSImage?, save: @escaping (Connection, String) -> Void,
         cancel: @escaping () -> Void) {
        _m = StateObject(wrappedValue: EditorModel(item))
        self.item = item
        self.thumbnail = thumbnail
        self.save = save
        self.cancel = cancel
    }

    private var parsed: URL? { Connection.normalize(m.url) }
    private var shortHost: String { parsed?.host?.components(separatedBy: ".").first ?? "" }
    private var insecure: Bool {
        guard let p = parsed else { return false }
        var c = item.connection
        c.url = p.absoluteString
        return !c.isSecureContext
    }

    private var title: String {
        if item.passwordOnly { return "Connect to \(item.connection.name)" }
        if !item.isNew { return "Edit \(item.connection.name)" }
        return "Add Computer"
    }

    private var subtitle: String {
        if item.passwordOnly || !item.isNew { return item.connection.host }
        return "Connect to a computer running voxl"
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            SheetHeader(symbol: item.symbol, image: thumbnail, title: title, subtitle: subtitle)
                .padding([.horizontal, .top], 24)
                .padding(.bottom, 8)
            Form {
                if !item.passwordOnly {
                    Section {
                        TextField("Address", text: $m.url, prompt: Text("https://mac.tailnet.ts.net:6443"))
                            .onChange(of: m.url) { _, new in
                                // Pasting voxl's sign-in link splits off the password.
                                let (clean, token) = Connection.split(link: new)
                                if let token {
                                    m.password = token
                                    m.url = clean
                                }
                            }
                        TextField("Name", text: $m.name, prompt: Text(shortHost.isEmpty ? "Mac mini" : shortHost))
                    } footer: {
                        Group {
                        if insecure {
                            Label("Use HTTPS: plain HTTP would send the password in the clear, and video only plays over HTTPS. Serve voxl with `tailscale serve` or `--tls`.",
                                  systemImage: "exclamationmark.triangle.fill")
                                .foregroundStyle(.orange)
                        } else if item.isNew {
                            Text("Tip: copy the sign-in link voxl prints and it fills in the address and password.")
                                .foregroundStyle(.secondary)
                        }
                        }
                        .frame(maxWidth: .infinity, alignment: .leading)
                    }
                }
                Section {
                    SecureField("Password", text: $m.password, prompt: Text("Required"))
                } footer: {
                    Text("The password voxl was started with (VOXL_PASSWORD). Saved in your Keychain.")
                        .foregroundStyle(.secondary)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
            }
            .formStyle(.grouped)
            .scrollDisabled(true)
            .frame(height: item.passwordOnly ? 120 : 250)
            HStack {
                Spacer()
                Button("Cancel", role: .cancel, action: cancel).keyboardShortcut(.cancelAction)
                Button(item.connectAfter ? "Connect" : item.isNew ? "Add" : "Save") {
                    var c = item.connection
                    c.url = Connection.split(link: m.url).url
                    if c.baseURL?.host != item.connection.baseURL?.host { c.certFingerprint = nil }
                    c.name = m.name.isEmpty ? (shortHost.isEmpty ? "Computer" : shortHost) : m.name
                    save(c, m.password)
                }
                .keyboardShortcut(.defaultAction)
                .disabled(parsed == nil || m.password.isEmpty || insecure)
            }
            .controlSize(.large)
            .padding(.horizontal, 24).padding(.bottom, 20).padding(.top, 4)
        }
        .frame(width: 460)
    }
}

// MARK: - Settings

final class AccessibilityStatus: ObservableObject {
    @Published var trusted = KeyCapture.shared.accessibilityGranted
}

struct SettingsView: View {
    var body: some View {
        TabView {
            KeyboardSettings()
                .tabItem { Label("Keyboard", systemImage: "keyboard") }
        }
        .frame(width: 500)
    }
}

private struct KeyboardSettings: View {
    @AppStorage(Prefs.captureAppShortcutsKey) private var appShortcuts = true
    @AppStorage(Prefs.captureSystemShortcutsKey) private var systemShortcuts = true
    @StateObject private var ax = AccessibilityStatus()
    private let timer = Timer.publish(every: 1.5, on: .main, in: .common).autoconnect()

    var body: some View {
        Form {
            Section {
                Toggle(isOn: $appShortcuts) {
                    Text("Send ⌘ shortcuts to the remote computer")
                    Text("⌘W, ⌘T, ⌘Q and the rest go to the computer you're viewing, not this Mac.")
                }
                Toggle(isOn: $systemShortcuts) {
                    Text("Send system shortcuts too")
                    Text("⌘Tab, ⌘Space, Raycast and Alfred hotkeys, Mission Control and screenshot keys.")
                }
                .disabled(!appShortcuts)
                .onChange(of: systemShortcuts) { _, on in
                    if on && !ax.trusted { KeyCapture.shared.requestAccessibility() }
                }
            } footer: {
                Text("Press ⌃⌥K in a viewer to switch between the remote computer and this Mac.")
                    .foregroundStyle(.secondary)
            }
            if systemShortcuts && appShortcuts {
                Section {
                    LabeledContent("Accessibility") {
                        if ax.trusted {
                            Label("Allowed", systemImage: "checkmark.circle.fill").foregroundStyle(.green)
                        } else {
                            Button("Allow in System Settings…") {
                                KeyCapture.shared.requestAccessibility()
                                NSWorkspace.shared.open(URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility")!)
                            }
                        }
                    }
                } footer: {
                    Text("macOS only lets an app take system shortcuts with Accessibility permission.")
                        .foregroundStyle(.secondary)
                }
            }
        }
        .formStyle(.grouped)
        .fixedSize(horizontal: false, vertical: true)
        .onReceive(timer) { _ in
            ax.trusted = KeyCapture.shared.accessibilityGranted
            KeyCapture.shared.refresh()
        }
    }
}

import AppKit
import SwiftUI
import WebKit

@main
enum Entry {
    static func main() {
        if CommandLine.arguments.contains("--selftest") {
            SelfTest.run()
        } else {
            VoxlViewerApp.main()
        }
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        KeyCapture.shared.start()
        // Versions before 0.1.1 kept web data on disk, including sign-in URLs. Clear it.
        if !UserDefaults.standard.bool(forKey: "clearedWebData") {
            WKWebsiteDataStore.default().removeData(ofTypes: WKWebsiteDataStore.allWebsiteDataTypes(),
                                                    modifiedSince: .distantPast) {
                UserDefaults.standard.set(true, forKey: "clearedWebData")
            }
        }
        // `--open <link>`: open a viewer straight away (handy for scripts and testing).
        let args = CommandLine.arguments
        // `--add <link>`: save a computer from voxl's sign-in link (password to the Keychain).
        if let i = args.firstIndex(of: "--add"), i + 1 < args.count {
            let (url, token) = Connection.split(link: args[i + 1])
            if !ConnectionStore.shared.contains(url: url) {
                let c = Connection(name: URL(string: url)?.host?.components(separatedBy: ".").first ?? "Computer", url: url)
                ConnectionStore.shared.upsert(c)
                c.password = token
            }
        }
        if let i = args.firstIndex(of: "--open"), i + 1 < args.count {
            let (url, token) = Connection.split(link: args[i + 1])
            let c = Connection(name: URL(string: url)?.host?.components(separatedBy: ".").first ?? "Computer", url: url)
            ViewerWindowController.show(c, password: token)
        }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }
}

struct VoxlViewerApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @StateObject private var store = ConnectionStore.shared

    var body: some Scene {
        Window("Voxl Viewer", id: "connections") {
            ConnectionsView().environmentObject(store)
        }
        .defaultSize(width: 900, height: 600)
        .windowToolbarStyle(.unified)
        .commands { AppCommands() }

        Settings { SettingsView() }
    }
}

/// Menus. Shortcuts with ⌘ reach these only while no viewer is capturing the keyboard.
struct AppCommands: Commands {
    @Environment(\.openWindow) private var openWindow

    var body: some Commands {
        CommandGroup(replacing: .newItem) {
            Button("Add Computer…") {
                openWindow(id: "connections")
                NotificationCenter.default.post(name: .addComputer, object: nil)
            }
            .keyboardShortcut("n")
            Button("Show Computers") { openWindow(id: "connections") }
                .keyboardShortcut("0", modifiers: [.command, .shift])
        }
        CommandGroup(after: .toolbar) {
            Button("Fit Window to Remote Screen") { ViewerWindowController.key?.fitWindow() }
                .keyboardShortcut("0", modifiers: [.command, .option])
            Divider()
        }
        CommandMenu("Connection") {
            Button("Use Keyboard on This Mac / Remote") { ViewerWindowController.key?.captureActive.toggle() }
                .keyboardShortcut("k", modifiers: [.control, .option])
            Divider()
            Button("Reconnect") { ViewerWindowController.key?.reload() }
                .keyboardShortcut("r")
            Button("Open in Browser") {
                guard let v = ViewerWindowController.key,
                      let u = v.connection.loadURL(password: v.connection.password) else { return }
                NSWorkspace.shared.open(u)
            }
        }
    }
}

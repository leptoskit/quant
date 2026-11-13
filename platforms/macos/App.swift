import AppKit
import LeptosKitBridge

// The resident core in quant_service: the daemon worker and the menu-bar item.
@_silgen_name("quant_desktop_start")
private func quantDesktopStart(_ cache: UnsafePointer<CChar>?) -> Int32
@_silgen_name("quant_desktop_stop")
private func quantDesktopStop()

/// Quant lives in the menu bar. The status item and everything that talks to
/// the daemon are native Rust, started here at launch. The "Open Quant"
/// window shows the leptoskit guest, created the first time it opens, so an
/// app that stays in the menu bar never loads it.
@main
@MainActor
final class MacApp: NSObject, NSApplicationDelegate, NSWindowDelegate {
    private var window: NSWindow?
    private var controller: LeptosKitMacViewController?
    private var terminating = false
    private static let windowSize = NSSize(width: 620, height: 780)
    private static let minimumWindowSize = NSSize(width: 420, height: 360)

    static func main() {
        let app = NSApplication.shared
        let delegate = MacApp()
        // No Dock icon or app menu, as LSUIElement in Info.plist also says.
        app.setActivationPolicy(.accessory)
        app.delegate = delegate
        app.run()
        withExtendedLifetime(delegate) {}
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        let caches = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask).first
            ?? FileManager.default.temporaryDirectory
        let cache = caches.appendingPathComponent(Bundle.main.bundleIdentifier ?? "Quant")
        guard quantDesktopStart(cache.path) == 0 else {
            fail(CocoaError(.featureUnsupported), title: "Quant couldn't start")
            return
        }
        Snapshots.startIfAsked(self)
    }

    /// Shows the window, creating it and its guest the first time. The core
    /// sends this action when "Open Quant…" is chosen, or to ask for the
    /// passphrase.
    @objc func openQuant(_ sender: Any?) {
        guard let window = window ?? makeWindow() else { return }
        if !window.isVisible { window.center() }
        window.makeKeyAndOrderFront(nil)
        NSApp.activate()
    }

    private func makeWindow() -> NSWindow? {
        do {
            guard let url = Bundle.main.url(forResource: "app", withExtension: "wasm") else {
                throw CocoaError(.fileNoSuchFile)
            }
            controller = try LeptosKitMacViewController(
                wasm: Data(contentsOf: url), register: GeneratedPlugins.register
            )
        } catch {
            fail(error, title: "Quant couldn't open its window")
            return nil
        }
        let window = NSWindow(
            contentRect: NSRect(origin: .zero, size: Self.windowSize),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered, defer: true
        )
        window.title = "Quant"
        window.contentMinSize = Self.minimumWindowSize
        window.isReleasedWhenClosed = false
        window.delegate = self
        window.contentViewController = controller
        window.setContentSize(Self.windowSize)
        self.window = window
        installMenu()
        return window
    }

    private func fail(_ error: Error, title: String) {
        NSLog("quant: %@", error.localizedDescription)
        let alert = NSAlert(error: error)
        alert.messageText = title
        alert.runModal()
    }

    /// Closing hides the window. Its guest stays loaded: freeing it would not
    /// give the memory back, and reopening is then instant.
    func windowShouldClose(_ sender: NSWindow) -> Bool {
        sender.orderOut(nil)
        return false
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows: Bool) -> Bool {
        openQuant(nil)
        return false
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }

    /// Quitting closes the window's guest, then the core, which ends the owner
    /// session.
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        guard !terminating else { return .terminateNow }
        terminating = true
        let controller = self.controller
        Task { @MainActor in
            do { try await controller?.closeAndWait() } catch {
                NSLog("quant: closing: %@", error.localizedDescription)
            }
            quantDesktopStop()
            NSApp.reply(toApplicationShouldTerminate: true)
        }
        return .terminateLater
    }

    /// Key equivalents for the window's text fields; an accessory app never
    /// shows this menu. It is installed with the window, not at launch: AppKit
    /// fills an Edit menu with Writing Tools, AutoFill, and Dictation, which
    /// load several hundred frameworks.
    private func installMenu() {
        let menu = NSMenu()
        let appItem = NSMenuItem()
        let app = NSMenu()
        app.addItem(withTitle: "Close Window", action: #selector(NSWindow.performClose(_:)), keyEquivalent: "w")
        appItem.submenu = app
        menu.addItem(appItem)

        let editItem = NSMenuItem()
        let edit = NSMenu(title: "Edit")
        edit.addItem(withTitle: "Undo", action: Selector(("undo:")), keyEquivalent: "z")
        edit.addItem(withTitle: "Redo", action: Selector(("redo:")), keyEquivalent: "Z")
        edit.addItem(.separator())
        edit.addItem(withTitle: "Cut", action: #selector(NSText.cut(_:)), keyEquivalent: "x")
        edit.addItem(withTitle: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c")
        edit.addItem(withTitle: "Paste", action: #selector(NSText.paste(_:)), keyEquivalent: "v")
        edit.addItem(withTitle: "Select All", action: #selector(NSText.selectAll(_:)), keyEquivalent: "a")
        editItem.submenu = edit
        menu.addItem(editItem)
        NSApp.mainMenu = menu
    }
}

/// A development aid: with `QUANT_SNAPSHOT=<directory>` set, the app draws
/// its own windows into PNGs there: the menu-bar menu, then the "Open Quant"
/// window on each section. It draws in-process, so it needs no Screen
/// Recording permission. Without the variable, none of this runs.
///
/// `QUANT_SNAPSHOT_STEPS` replaces the default walk with `;`-separated steps:
/// `press:<button>`, `capture:<name>`, `menu` (open and draw the menu-bar
/// menu), and `unlock` (type `QUANT_SNAPSHOT_UNLOCK`, a scratch vault's
/// passphrase, and press Unlock).
@MainActor
enum Snapshots {
    private static var directory: URL?
    private static var count = 0
    private static let environment = ProcessInfo.processInfo.environment
    private static let defaultSteps =
        "capture:status;press:Saved moments;capture:moments;press:Agents;capture:agents;"
        + "press:Audit;capture:audit"

    static func startIfAsked(_ app: MacApp) {
        guard let path = environment["QUANT_SNAPSHOT"], !path.isEmpty else { return }
        if let appearance = environment["QUANT_SNAPSHOT_APPEARANCE"] {
            NSApp.appearance = NSAppearance(named: appearance == "light" ? .aqua : .darkAqua)
        }
        let directory = URL(fileURLWithPath: path, isDirectory: true)
        try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        self.directory = directory
        let delay = Double(environment["QUANT_SNAPSHOT_DELAY"] ?? "") ?? 4
        DispatchQueue.main.asyncAfter(deadline: .now() + delay) {
            openMenu()
            DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) {
                app.openQuant(nil)
                run(steps(), app: app)
            }
        }
    }

    private static func steps() -> [() -> Void] {
        let script = environment["QUANT_SNAPSHOT_STEPS"] ?? defaultSteps
        return script.split(separator: ";").map { step in
            let parts = step.split(separator: ":", maxSplits: 1).map(String.init)
            let argument = parts.count > 1 ? parts[1] : ""
            switch parts.first ?? "" {
            case "press": return { press(argument) }
            case "capture": return { capture(argument) }
            case "menu": return { openMenu() }
            case "unlock":
                return {
                    enter(environment["QUANT_SNAPSHOT_UNLOCK"] ?? "")
                    press("Unlock")
                }
            default: return { NSLog("quant snapshot: unknown step %@", String(step)) }
            }
        }
    }

    private static func run(_ steps: [() -> Void], app: MacApp) {
        guard let step = steps.first else { return }
        DispatchQueue.main.asyncAfter(deadline: .now() + 2) {
            step()
            run(Array(steps.dropFirst()), app: app)
        }
    }

    private static func views(_ root: NSView) -> [NSView] {
        [root] + root.subviews.flatMap(views)
    }

    private static func windowViews() -> [NSView] {
        NSApp.windows.filter { $0.title == "Quant" }.compactMap(\.contentView).flatMap(views)
    }

    private static func press(_ title: String) {
        let button = windowViews().compactMap { $0 as? NSButton }.first {
            $0.title == title || $0.title.hasSuffix(" \(title)")
        }
        guard let button else { return NSLog("quant snapshot: no %@ button", title) }
        button.performClick(nil)
    }

    private static func enter(_ text: String) {
        guard let field = windowViews().compactMap({ $0 as? NSSecureTextField }).first else {
            return NSLog("quant snapshot: no passphrase field")
        }
        field.stringValue = text
        let delegate = field.delegate
        delegate?.controlTextDidChange?(
            Notification(name: NSControl.textDidChangeNotification, object: field)
        )
    }

    /// Clicks the status item, captures the menu while it tracks, then closes it.
    private static func openMenu() {
        guard let statusWindow = NSApp.windows.first(where: {
            String(describing: type(of: $0)).contains("StatusBarWindow")
        }), let button = statusWindow.contentView else {
            NSLog("quant snapshot: no status item")
            return
        }
        capture("menubar-item", view: button)
        let timer = Timer(timeInterval: 1.0, repeats: false) { _ in
            MainActor.assumeIsolated {
                for window in NSApp.windows where window.isVisible && window !== statusWindow {
                    let name = String(describing: type(of: window))
                    if name.contains("Menu"), let view = window.contentView {
                        capture("menu", view: view)
                    }
                }
                let escape = NSEvent.keyEvent(
                    with: .keyDown, location: .zero, modifierFlags: [], timestamp: 0,
                    windowNumber: 0, context: nil, characters: "\u{1b}",
                    charactersIgnoringModifiers: "\u{1b}", isARepeat: false, keyCode: 53
                )
                if let escape { NSApp.postEvent(escape, atStart: false) }
            }
        }
        RunLoop.main.add(timer, forMode: .common)
        let point = NSPoint(x: button.bounds.midX, y: button.bounds.midY)
        let location = button.convert(point, to: nil)
        for type in [NSEvent.EventType.leftMouseDown, .leftMouseUp] {
            if let event = NSEvent.mouseEvent(
                with: type, location: location, modifierFlags: [], timestamp: 0,
                windowNumber: statusWindow.windowNumber, context: nil, eventNumber: 0,
                clickCount: 1, pressure: 1
            ) {
                NSApp.postEvent(event, atStart: false)
            }
        }
    }

    static func capture(_ name: String, view: NSView? = nil) {
        guard let directory else { return }
        guard let view = view ?? NSApp.keyWindow?.contentView ?? NSApp.windows.first(where: {
            $0.isVisible && $0.title == "Quant"
        })?.contentView else {
            NSLog("quant snapshot: nothing to draw for %@", name)
            return
        }
        guard let bitmap = view.bitmapImageRepForCachingDisplay(in: view.bounds) else { return }
        view.cacheDisplay(in: view.bounds, to: bitmap)
        count += 1
        let url = directory.appendingPathComponent(String(format: "%02d-%@.png", count, name))
        do {
            try bitmap.representation(using: .png, properties: [:])?.write(to: url)
            NSLog("quant snapshot: %@", url.path)
        } catch {
            NSLog("quant snapshot: %@", error.localizedDescription)
        }
    }
}

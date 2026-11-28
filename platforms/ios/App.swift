import UIKit
import LeptosKitBridge
#if canImport(LeptosKitAppShortcuts)
import LeptosKitAppShortcuts
#endif
#if canImport(LeptosKitBackground)
import LeptosKitBackground
#endif
#if canImport(LeptosKitNotifications)
import LeptosKitNotifications
#endif

// The host leptoskit generates for iOS apps (its native/templates/App.swift),
// with the background plugin's launch registration moved to
// `application(_:willFinishLaunchingWithOptions:)`, as that template does,
// and one thing of Quant's own in
// `application(_:didFinishLaunchingWithOptions:)`: nothing the app stores is
// backed up.
@main
final class AppDelegate: UIResponder, UIApplicationDelegate {
    func application(
        _ application: UIApplication,
        willFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil
    ) -> Bool {
        #if canImport(LeptosKitBackground)
        // iOS ends an app that registers a background task's handler after
        // launch finishes, so the handlers go in before this returns; the
        // screen's `start` only schedules the next app refresh.
        BackgroundPlugin.registerLaunchHandlers()
        #endif

        return true
    }

    func application(
        _ application: UIApplication,
        didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]?
    ) -> Bool {
        excludeAppDataFromBackup()
        if let url = launchOptions?[.url] as? URL {
            QuantLinkPlugin.claim(url)
        }
        return true
    }

    /// Receives a `quant://link` on the app delegate path. The scene delegate
    /// also handles it for scene-based launches, and the plugin drops the
    /// duplicate.
    func application(
        _ application: UIApplication,
        open url: URL,
        options: [UIApplication.OpenURLOptionsKey: Any] = [:]
    ) -> Bool {
        QuantLinkPlugin.claim(url)
    }

    /// The phone's signing identity, its settings and the encrypted spool of
    /// readings live in Application Support, where the `quant-phone` service
    /// keeps its files. The spool key is in this iPhone's keychain and never
    /// leaves it, so a restored copy would be a second phone claiming to be
    /// the first, with a spool it could not open. As on Android, none of it
    /// goes into an iCloud or computer backup.
    private func excludeAppDataFromBackup() {
        do {
            var support = try FileManager.default.url(
                for: .applicationSupportDirectory, in: .userDomainMask,
                appropriateFor: nil, create: true
            )
            var values = URLResourceValues()
            values.isExcludedFromBackup = true
            try support.setResourceValues(values)
        } catch {
            NativeLog.error(NativeLog.bridge, "cannot exclude app data from backup", error)
        }
    }

    func application(
        _ application: UIApplication,
        configurationForConnecting session: UISceneSession,
        options: UIScene.ConnectionOptions
    ) -> UISceneConfiguration {
        let configuration = UISceneConfiguration(name: "Main", sessionRole: session.role)
        configuration.delegateClass = SceneDelegate.self
        return configuration
    }

    func application(
        _ application: UIApplication,
        performActionFor shortcutItem: UIApplicationShortcutItem,
        completionHandler: @escaping (Bool) -> Void
    ) {
        #if canImport(LeptosKitAppShortcuts)
        AppShortcutsPlugin.noteLaunchIfPresent(id: shortcutItem.type)
        #endif
        completionHandler(true)
    }

    func application(
        _ application: UIApplication,
        didRegisterForRemoteNotificationsWithDeviceToken deviceToken: Data
    ) {
        #if canImport(LeptosKitNotifications)
        NotificationsPlugin.noteDeviceToken(deviceToken)
        #endif
    }

    func application(
        _ application: UIApplication,
        didFailToRegisterForRemoteNotificationsWithError error: any Error
    ) {
        #if canImport(LeptosKitNotifications)
        NotificationsPlugin.noteRegistrationFailure(error)
        #endif
    }
}

final class SceneDelegate: UIResponder, UIWindowSceneDelegate {
    var window: UIWindow?

    func scene(
        _ scene: UIScene,
        willConnectTo session: UISceneSession,
        options: UIScene.ConnectionOptions
    ) {
        guard let scene = scene as? UIWindowScene else { return }

        let window = UIWindow(windowScene: scene)
        installController(in: window)
        self.window = window
        window.makeKeyAndVisible()

        // A cold launch by URL arrives on the scene's connection options
        // rather than through `openURLContexts`.
        for context in options.urlContexts {
            QuantLinkPlugin.claim(context.url)
        }

        #if canImport(LeptosKitAppShortcuts)

        if let shortcut = options.shortcutItem {
            AppShortcutsPlugin.noteLaunchIfPresent(id: shortcut.type)
        }

        #endif
    }

    /// Receives a `quant://link` while this scene is running or returning to
    /// the foreground.
    func scene(
        _ scene: UIScene,
        openURLContexts URLContexts: Set<UIOpenURLContext>
    ) {
        for context in URLContexts {
            QuantLinkPlugin.claim(context.url)
        }
    }

    func windowScene(
        _ windowScene: UIWindowScene,
        performActionFor shortcutItem: UIApplicationShortcutItem,
        completionHandler: @escaping (Bool) -> Void
    ) {
        #if canImport(LeptosKitAppShortcuts)
        AppShortcutsPlugin.noteLaunchIfPresent(id: shortcutItem.type)
        #endif
        completionHandler(true)
    }

    private func installController(in window: UIWindow) {
        do {
            window.rootViewController = try applicationController()
        } catch {
            NativeLog.error(NativeLog.bridge, "cannot start the app", error)
            let controller = UIViewController()
            let label = UILabel()
            label.text = error.localizedDescription
            label.numberOfLines = 0
            label.accessibilityIdentifier = "leptoskit-error"
            controller.view = label
            window.rootViewController = controller
        }
    }

    private func applicationController() throws -> LeptosKitApplicationController {
        guard let embedded = Bundle.main.url(forResource: "app", withExtension: "lkb"),
              let configuration = Bundle.main.url(
                  forResource: "codepush", withExtension: "json"
              ) else {
            throw CocoaError(.fileNoSuchFile)
        }

        return try LeptosKitApplicationController(
            embedded: Data(contentsOf: embedded), configuration: Data(contentsOf: configuration),
            register: registerPlugins
        )
    }

    func sceneDidDisconnect(_ scene: UIScene) {
        (window?.rootViewController as? LeptosKitApplicationController)?.close()
        window = nil
    }
}

/// Registers the generated plugins and the app's own inbound-link holder.
@MainActor
private func registerPlugins(
    _ bridge: OpaquePointer,
    _ dispatcher: NativeDispatcher,
    _ context: NativePluginContext
) throws {
    try GeneratedPlugins.register(bridge, dispatcher, context)
    // App-owned: the generated plugin set has no inbound link handler.
    try dispatcher.register(QuantLinkPlugin(), on: bridge)
}

/// Holds one inbound `quant://link` for the screen to review.
///
/// The OS hands the URL to the app and scene delegates; this keeps the latest
/// one until the guest asks for it. Receiving a link only asks the screen to
/// show its Connect card. The host never pairs, replaces a vault, or changes
/// collection state, and it never logs the URL.
@MainActor
private final class QuantLinkPlugin: NativePlugin {
    // Must match quant_model::phone::{LINK_PLUGIN, LINK_NEXT}.
    let name = "quant-link"
    let methods: [UInt32] = [QuantLinkPlugin.next]

    private static let next: UInt32 = 1
    private static let maxBytes = 4096
    /// iOS can report one opening through both app and scene callbacks.
    private static let duplicateWindow: TimeInterval = 1
    private static var pending: String?
    private static var waiter: CheckedContinuation<String?, Never>?
    private static var last: String?
    private static var lastAt: TimeInterval?

    func invoke(method: UInt32, payload: NativeBuffer) async throws -> Data {
        guard method == Self.next else { throw PluginError.unavailable }
        guard payload.count == 0 else { throw PluginError.invalidArgument }
        guard let link = await Self.take() else { throw PluginError.cancelled }
        return Data(link.utf8)
    }

    func close() {
        Self.resume(nil)
    }

    /// Claims an inbound URL when it is this app's pairing link.
    @discardableResult
    static func claim(_ url: URL?) -> Bool {
        guard let url,
              url.scheme?.lowercased() == "quant",
              url.host?.lowercased() == "link"
        else { return false }
        let link = url.absoluteString
        guard link.utf8.count <= maxBytes,
              !link.unicodeScalars.contains(where: CharacterSet.controlCharacters.contains)
        else { return false }
        // The app delegate and scene delegate can both see one opening.
        guard pending != link else { return true }
        let now = ProcessInfo.processInfo.systemUptime
        if last == link, let lastAt, now - lastAt < duplicateWindow {
            return true
        }
        Self.last = link
        Self.lastAt = now
        if let waiter {
            Self.waiter = nil
            waiter.resume(returning: link)
        } else {
            pending = link
        }
        return true
    }

    private static func take() async -> String? {
        if let pending {
            Self.pending = nil
            return pending
        }
        return await withCheckedContinuation { continuation in
            if let stale = waiter {
                stale.resume(returning: nil)
            }
            waiter = continuation
        }
    }

    private static func resume(_ value: String?) {
        guard let waiter else { return }
        Self.waiter = nil
        waiter.resume(returning: value)
    }
}

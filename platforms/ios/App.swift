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
// with the background plugin's launch registration in
// `application(_:didFinishLaunchingWithOptions:)`, plus one thing of Quant's
// own there: nothing the app stores is backed up.
@main
final class AppDelegate: UIResponder, UIApplicationDelegate {
    func application(
        _ application: UIApplication,
        didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]?
    ) -> Bool {
        #if canImport(LeptosKitBackground)
        // iOS ends an app that registers a background task's handler after
        // launch finishes, so the handlers go in before this returns; the
        // screen's `start` only schedules the next app refresh.
        BackgroundPlugin.registerLaunchHandlers()
        #endif
        excludeAppDataFromBackup()
        return true
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

        #if canImport(LeptosKitAppShortcuts)

        if let shortcut = options.shortcutItem {
            AppShortcutsPlugin.noteLaunchIfPresent(id: shortcut.type)
        }

        #endif
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
            register: GeneratedPlugins.register
        )
    }

    func sceneDidDisconnect(_ scene: UIScene) {
        (window?.rootViewController as? LeptosKitApplicationController)?.close()
        window = nil
    }
}

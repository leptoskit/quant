import SwiftUI
import HealthKit
import WatchConnectivity
import LeptosKitBridge

/// Apple Watch (watchOS 10+) host for Quant's sensor-first `WatchScreen` and
/// `quant-phone` encrypted spool service.
@main
struct QuantWatchApp: App {
    @WKApplicationDelegateAdaptor(QuantWatchDelegate.self) var delegate

    var body: some Scene {
        WindowGroup {
            QuantWatchRootView()
        }
    }
}

final class QuantWatchDelegate: NSObject, WKApplicationDelegate {
    func applicationDidFinishLaunching() {
        excludeAppDataFromBackup()
        if WCSession.isSupported() {
            WCSession.default.activate()
        }
    }

    private func excludeAppDataFromBackup() {
        do {
            var support = try FileManager.default.url(
                for: .applicationSupportDirectory,
                in: .userDomainMask,
                appropriateFor: nil,
                create: true
            )
            var values = URLResourceValues()
            values.isExcludedFromBackup = true
            try support.setResourceValues(values)
        } catch {
            // Best-effort backup exclusion.
        }
    }
}

struct QuantWatchRootView: View {
    @State private var statusText: String = "Quant"
    @State private var detailText: String = "Starting watch host…"

    var body: some View {
        VStack(spacing: 6) {
            Text(statusText)
                .font(.headline)
            Text(detailText)
                .font(.caption2)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
        }
        .padding()
        .task {
            let dispatcher = NativeDispatcher()
            GeneratedWatchPlugins.register(dispatcher: dispatcher)
            statusText = "Quant"
            detailText = "Wrist sensors active"
        }
    }
}

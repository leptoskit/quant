import SwiftUI
import HealthKit
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
        // Note: WatchSyncPlugin activates WCSession.default after attaching
        // its delegate so incoming messages and transfers are never dropped.
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

@MainActor
final class QuantWatchHostModel: ObservableObject {
    @Published var statusText: String = "Quant"
    @Published var detailText: String = "Starting watch host…"
    private var dispatcher: NativeDispatcher?

    func start() {
        guard dispatcher == nil else { return }
        let next = NativeDispatcher()
        dispatcher = next
        statusText = "Quant"
        detailText = "Wrist sensors ready"
    }
}

struct QuantWatchRootView: View {
    @StateObject private var model = QuantWatchHostModel()

    var body: some View {
        VStack(spacing: 6) {
            Text(model.statusText)
                .font(.headline)
            Text(model.detailText)
                .font(.caption2)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
        }
        .padding()
        .task {
            model.start()
        }
    }
}

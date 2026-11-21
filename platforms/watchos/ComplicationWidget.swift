import SwiftUI
import WidgetKit

/// Smart Stack glance and watch-face circular complication for Quant on watchOS.
struct QuantComplicationEntry: TimelineEntry {
    let date: Date
    let badge: String
    let headline: String
}

struct QuantComplicationProvider: TimelineProvider {
    func placeholder(in context: Context) -> QuantComplicationEntry {
        QuantComplicationEntry(date: .now, badge: "72", headline: "Recording")
    }

    func getSnapshot(in context: Context, completion: @escaping (QuantComplicationEntry) -> Void) {
        completion(loadEntry())
    }

    func getTimeline(
        in context: Context,
        completion: @escaping (Timeline<QuantComplicationEntry>) -> Void
    ) {
        let entry = loadEntry()
        let next = Calendar.current.date(byAdding: .minute, value: 15, to: .now) ?? .now
        completion(Timeline(entries: [entry], policy: .after(next)))
    }

    private static let appGroupSuite = "group.dev.leptoskit.quant"

    private func loadEntry() -> QuantComplicationEntry {
        let defaults = UserDefaults(suiteName: Self.appGroupSuite) ?? UserDefaults.standard
        let badge = defaults.string(forKey: "quant.watch.badge") ?? "REC"
        let headline = defaults.string(forKey: "quant.watch.headline") ?? "Quant"
        return QuantComplicationEntry(date: .now, badge: badge, headline: headline)
    }
}

struct QuantComplicationView: View {
    let entry: QuantComplicationEntry
    @Environment(\.widgetFamily) private var family

    var body: some View {
        switch family {
        case .accessoryCircular:
            VStack(spacing: 1) {
                Text("Q").font(.caption2.bold())
                Text(entry.badge).font(.caption.monospacedDigit())
            }
        default:
            VStack(alignment: .leading, spacing: 2) {
                Text("Quant").font(.caption2.bold())
                Text(entry.headline).font(.caption)
            }
        }
    }
}

struct QuantWatchComplication: Widget {
    let kind: String = "dev.leptoskit.quant.complication"

    var body: some WidgetConfiguration {
        StaticConfiguration(kind: kind, provider: QuantComplicationProvider()) { entry in
            QuantComplicationView(entry: entry)
        }
        .configurationDisplayName("Quant")
        .description("Shows Quant watch recording status and latest heart rate.")
        .supportedFamilies([.accessoryCircular, .accessoryRectangular, .accessoryInline])
    }
}

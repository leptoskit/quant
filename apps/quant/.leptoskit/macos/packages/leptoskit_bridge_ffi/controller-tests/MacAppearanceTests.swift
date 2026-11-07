@testable import LeptosKitMacControllers
import AppKit
import Testing

@Suite(.serialized)
@MainActor
struct MacAppearanceTests {
    /// The controller passes light or dark to the guest when it lays out, so a
    /// switch must ask for a layout even when the size stays the same.
    @Test func appearanceChangesAskForALayout() {
        let view = FlippedView(frame: NSRect(x: 0, y: 0, width: 320, height: 480))
        view.appearance = NSAppearance(named: .aqua)
        view.layoutSubtreeIfNeeded()
        #expect(!view.needsLayout)

        view.appearance = NSAppearance(named: .darkAqua)
        #expect(view.needsLayout)
    }
}

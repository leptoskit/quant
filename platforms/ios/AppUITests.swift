import XCTest

@MainActor
final class AppUITests: XCTestCase {
    private static let launchTimeout: TimeInterval = 15

    /// The app opens on the phone screen, whose title a screen reader can
    /// reach before anything is paired or allowed.
    func testOpensOnThePhoneScreen() {
        let app = XCUIApplication()
        app.launch()
        XCTAssertTrue(app.staticTexts["quant"].waitForExistence(timeout: Self.launchTimeout))
        XCTAssertTrue(app.staticTexts["YOUR PRIVATE MEMORY"].waitForExistence(timeout: 10))
    }
}

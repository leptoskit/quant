// swift-tools-version:6.0
import PackageDescription
let package = Package(
name: "App", platforms: [.macOS(.v14)],
dependencies: [.package(path: "packages/leptoskit_bridge_ffi"),
],
targets: [.executableTarget(
name: "App",
dependencies: [.product(name: "LeptosKitBridge", package: "leptoskit_bridge_ffi"),
],
path: "Sources",
sources: ["App.swift",
"GeneratedPlugins.swift",
"NativeRive.swift",
"LeptosKitMacViewController.swift",
"MacWidgetRenderer.swift",
"MacWidgetControls.swift",
"MacTextLabel.swift",
"MacScrollView.swift",
"DevtoolsOverlay.swift",
"RenderTransaction.swift",
"WidgetProps.swift",
"WidgetMotion.swift",
"LayerMotion.swift",
"ExitingViews.swift",
"ScrollTracker.swift",
"NativeRustWorker.swift",
"NativeRustAsync.swift",
"NativeAppFactory.swift",
"NativeAppRuntime.swift",
"NativeRequestExecutor.swift",
"NativeDevelopmentGuests.swift"]
)])

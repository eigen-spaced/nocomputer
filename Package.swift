// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "NoComputer",
    platforms: [.macOS(.v14)],
    products: [.executable(name: "NoComputerDiagnostics", targets: ["NoComputerDiagnostics"])],
    targets: [
        .executableTarget(name: "NoComputerDiagnostics"),
        .testTarget(name: "NoComputerDiagnosticsTests", dependencies: ["NoComputerDiagnostics"]),
    ],
    // Keep the initial Objective-C delegate prototype in Swift 5 language mode.
    // Capture callbacks are serialized and shared state is protected by a lock.
    swiftLanguageModes: [.v5]
)

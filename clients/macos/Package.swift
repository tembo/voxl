// swift-tools-version:6.0
import PackageDescription

let package = Package(
    name: "VoxlViewer",
    platforms: [.macOS(.v14)],
    targets: [
        .executableTarget(
            name: "VoxlViewer",
            path: "Sources/VoxlViewer",
            swiftSettings: [.swiftLanguageMode(.v5)]
        )
    ]
)

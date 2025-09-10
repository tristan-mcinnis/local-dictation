// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "ParakeetCLI",
    platforms: [
        .macOS(.v14)
    ],
    products: [
        .executable(
            name: "parakeet-cli",
            targets: ["ParakeetCLI"]
        )
    ],
    dependencies: [],
    targets: [
        .executableTarget(
            name: "ParakeetCLI",
            dependencies: [],
            path: "Sources"
        )
    ]
)
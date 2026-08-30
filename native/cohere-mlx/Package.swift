// swift-tools-version: 6.2
import PackageDescription

let package = Package(
    name: "CohereMLXBridge",
    platforms: [.macOS(.v14)],
    products: [
        .library(name: "CohereMLXBridgeProtocol", targets: ["CohereMLXBridgeProtocol"]),
        .executable(name: "cohere-mlx-bridge", targets: ["cohere-mlx-bridge"]),
    ],
    dependencies: [
        // Cohere Transcribe support is maintained upstream. Keep this as a
        // source dependency so model/runtime fixes are picked up together.
        .package(url: "https://github.com/Blaizzy/mlx-audio-swift.git", branch: "main"),
        // MLX is imported directly for constructing the f32 audio array.
        .package(url: "https://github.com/ml-explore/mlx-swift.git", .upToNextMajor(from: "0.30.6")),
    ],
    targets: [
        .target(name: "CohereMLXBridgeProtocol", path: "Sources/CohereMLXBridgeProtocol"),
        .executableTarget(
            name: "cohere-mlx-bridge",
            dependencies: [
                "CohereMLXBridgeProtocol",
                .product(name: "MLXAudioSTT", package: "mlx-audio-swift"),
                .product(name: "MLXAudioCore", package: "mlx-audio-swift"),
                .product(name: "MLX", package: "mlx-swift"),
            ],
            path: "Sources/CohereMLXBridge"
        ),
        .testTarget(
            name: "CohereMLXBridgeProtocolTests",
            dependencies: ["CohereMLXBridgeProtocol"],
            path: "Tests/CohereMLXBridgeProtocolTests"
        ),
    ]
)

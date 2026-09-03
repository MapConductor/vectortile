// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "MapConductorVectorTile",
    platforms: [.iOS(.v15), .macOS(.v12)],
    products: [
        .library(name: "MapConductorVectorTile", targets: ["MapConductorVectorTile"]),
    ],
    targets: [
        // Built by ../../scripts/build-ios.sh, then packaged with
        // `xcodebuild -create-xcframework`. Only the simulator slice is present;
        // shipping to devices means adding the aarch64-apple-ios target.
        .binaryTarget(name: "MvtRender", path: "MvtRender.xcframework"),
        .target(name: "MapConductorVectorTile", dependencies: ["MvtRender"]),
        .testTarget(
            name: "MapConductorVectorTileTests",
            dependencies: ["MapConductorVectorTile"],
            resources: [.process("Resources")],
        ),
    ],
)

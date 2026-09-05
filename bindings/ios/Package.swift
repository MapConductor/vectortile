// swift-tools-version: 5.9
import Foundation
import PackageDescription

// The Metal path encodes through MapConductorCore's TilePngEncoder rather than
// carrying its own copy of the encoder, so this package needs the core. The
// SDK checkout next door is preferred when present — that is what
// scripts/sync-sdk.sh keeps in step — and the published package is the
// fallback for a standalone clone.
let localCore = "../../../ios-sdk/ios-sdk-core"
let usingLocalCore = FileManager.default.fileExists(atPath: "\(localCore)/Package.swift")
let coreDependency: Package.Dependency = usingLocalCore
    ? .package(path: localCore)
    : .package(url: "https://github.com/MapConductor/ios-sdk-core", from: "1.0.0")

let package = Package(
    name: "MapConductorVectorTile",
    // "15.1", not .v15: ios-sdk-core declares 15.1 to dodge a Swift ABI bug in
    // 15.0, and a dependent cannot support less than what it depends on.
    platforms: [.iOS("15.1"), .macOS(.v12)],
    products: [
        .library(name: "MapConductorVectorTile", targets: ["MapConductorVectorTile"]),
    ],
    dependencies: [
        coreDependency,
    ],
    targets: [
        // Built by ../../scripts/build-ios.sh, then packaged with
        // `xcodebuild -create-xcframework`. Both slices are present: the
        // simulator for development and the device slice for real hardware.
        .binaryTarget(name: "MvtRender", path: "MvtRender.xcframework"),
        .target(
            name: "MapConductorVectorTile",
            dependencies: [
                .product(name: "MapConductorCore", package: "ios-sdk-core"),
                "MvtRender",
            ]
        ),
        .testTarget(
            name: "MapConductorVectorTileTests",
            dependencies: ["MapConductorVectorTile"],
            resources: [.process("Resources")],
        ),
    ],
)

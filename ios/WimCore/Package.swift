// swift-tools-version:5.9
import PackageDescription

// Built by ios/build-rust.sh (CI): the xcframework and the generated
// Sources/WimCore/wim_mobile.swift are not committed. The binary target
// is named after the module the generated Swift imports (wim_mobileFFI).
let package = Package(
    name: "WimCore",
    platforms: [.iOS(.v16)],
    products: [.library(name: "WimCore", targets: ["WimCore"])],
    targets: [
        .binaryTarget(name: "wim_mobileFFI", path: "WimCore.xcframework"),
        .target(name: "WimCore", dependencies: ["wim_mobileFFI"]),
        .testTarget(name: "WimCoreTests", dependencies: ["WimCore"]),
    ]
)

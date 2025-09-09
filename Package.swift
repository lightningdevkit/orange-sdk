// swift-tools-version:5.5
// The swift-tools-version declares the minimum version of Swift required to build this package.
import PackageDescription

let package = Package(
    name: "orange-sdk",
    platforms: [
        .iOS(.v15),
        .macOS(.v12),
    ],
    products: [
        // Products define the executables and libraries a package produces, and make them visible to other packages.
        .library(
            name: "OrangeSDK",
            targets: ["OrangeSDKFFI", "OrangeSDK"]),
    ],
    targets: [
        .target(
            name: "OrangeSDK",
            dependencies: ["OrangeSDKFFI"],
            path: "bindings/swift/Sources/OrangeSDK"
        ),
        .binaryTarget(
            name: "OrangeSDKFFI",
            url: "https://github.com/lightningdevkit/orange-sdk/releases/download/v0.1.0/OrangeSDKFFI.xcframework.zip",
            checksum: "00c43f6f9e88732e58d6923fdfbbab5567233a8a4e766a3e3429840ec4e18d04"
            )
    ]
)
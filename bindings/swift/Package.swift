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
            dependencies: ["OrangeSDKFFI"]
        ),
        .binaryTarget(
            name: "OrangeSDKFFI",
            path: "./OrangeSDKFFI.xcframework"
            )
    ]
)
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
            url: "https://github.com/lightningdevkit/orange-sdk/releases/download/v0.1.0-alpha.0/OrangeSDKFFI.xcframework.zip",
            checksum: "b4830908bb71a6878974d3e2984cdef23f01b1fbe2b19bc209cef27df12a15d5"
            )
    ]
)
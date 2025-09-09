# Swift Bindings Release Guide

This document outlines the steps to generate and release Swift bindings for orange-sdk.

## Prerequisites

- Rust toolchain with required targets installed
- Xcode and Swift toolchain
- Python 3 for utility scripts

## Generating Swift Bindings

### 1. Generate Bindings and XCFramework

Run the main generation script:

```bash
./scripts/uniffi_bindgen_generate_swift.sh
```

This script will:
- Build the Rust library with UniFFI features
- Generate Swift bindings using uniffi-bindgen
- Install required Rust targets for all Apple platforms
- Build for multiple architectures:
  - `aarch64-apple-darwin` (macOS Apple Silicon)
  - `x86_64-apple-darwin` (macOS Intel)
  - `aarch64-apple-ios` (iOS devices)
  - `x86_64-apple-ios` (iOS simulator Intel)
  - `aarch64-apple-ios-sim` (iOS simulator Apple Silicon)
- Create universal binaries using `lipo`
- Generate XCFramework with proper structure
- Add SystemConfiguration import to Swift bindings
- Move Swift source to proper package structure

### 2. Create Release Archive

Create the XCFramework archive and compute checksum:

```bash
./scripts/swift_create_xcframework_archive.sh
```

This creates:
- `bindings/swift/OrangeSDKFFI.xcframework.zip` (the release artifact)
- Computes SHA256 checksum for Swift Package Manager

### 3. Update Package.swift Files for Distribution

You need to maintain TWO Package.swift files:

1. **`bindings/swift/Package.swift`** - For local development (uses local path)
2. **Root `Package.swift`** - For public distribution (uses GitHub release URL)

#### Update Root Package.swift

Copy the bindings Package.swift to root and update it:

```bash
cp bindings/swift/Package.swift Package.swift
```

Then update the root Package.swift with:
- GitHub release URL instead of local path
- Path to Swift sources: `path: "bindings/swift/Sources/OrangeSDK"`

Change from:
```swift
.binaryTarget(
    name: "OrangeSDKFFI",
    path: "./OrangeSDKFFI.xcframework"
)
```

To:
```swift
.binaryTarget(
    name: "OrangeSDKFFI",
    url: "https://github.com/lightningdevkit/orange-sdk/releases/download/v0.1.0/OrangeSDKFFI.xcframework.zip",
    checksum: "COMPUTED_CHECKSUM_HERE"
)
```

And add the source path:
```swift
.target(
    name: "OrangeSDK",
    dependencies: ["OrangeSDKFFI"],
    path: "bindings/swift/Sources/OrangeSDK"
),
```

#### Keep Both Files

- **`bindings/swift/Package.swift`** - Used for local development and testing
- **`Package.swift`** - Used when developers add the entire repository as a dependency

## Release Process

### 1. Upload Release Asset

Upload the XCFramework archive to GitHub:
- File: `bindings/swift/OrangeSDKFFI.xcframework.zip` (typically ~330MB)
- Copy to Downloads: `cp bindings/swift/OrangeSDKFFI.xcframework.zip ~/Downloads/`
- Create GitHub release (e.g., `v0.1.0`)
- Upload the zip file as a release asset

### 2. Commit Package.swift Changes

Commit both Package.swift files:

```bash
git add Package.swift bindings/swift/Package.swift
git commit -m "Update Swift Package.swift files for v0.1.0 release"
git push origin master
```

### 3. Tag the Release

Tag the release to match the URL in Package.swift:

```bash
git tag v0.1.0
git push origin v0.1.0
```

## Generated Files Structure

After generation, you'll have:

```
bindings/swift/
├── Package.swift                           # Swift Package Manager config
├── Sources/OrangeSDK/OrangeSDK.swift      # Main Swift bindings
├── OrangeSDKFFI.xcframework/              # Multi-platform framework
│   ├── ios-arm64/                         # iOS devices
│   ├── ios-arm64_x86_64-simulator/        # iOS simulators  
│   └── macos-arm64_x86_64/                # macOS universal
├── OrangeSDKFFI.xcframework.zip           # Release artifact
└── liborange_sdk.dylib                    # Development library
```

## Usage for Developers

Once released, developers can integrate orange-sdk into their Swift projects:

### Swift Package Manager

Add to `Package.swift`:
```swift
dependencies: [
    .package(url: "https://github.com/lightningdevkit/orange-sdk", from: "0.1.0")
]
```

### Xcode

1. File → Add Package Dependencies
2. Enter repository URL: `https://github.com/lightningdevkit/orange-sdk`
3. Select version and add to target

### Import in Swift Code

```swift
import OrangeSDK

// Use orange-sdk APIs
let wallet = Wallet(...)
```

## Important Notes

- The XCFramework includes binaries for all Apple platforms
- The checksum in Package.swift must match the uploaded zip file exactly  
- Swift bindings require iOS 15+ and macOS 12+
- SystemConfiguration import is automatically added for network functionality
- The release process follows the same pattern as ldk-node

## Troubleshooting

### Compilation Issues
- Ensure all Rust targets are installed: `rustup target add aarch64-apple-ios x86_64-apple-ios aarch64-apple-ios-sim aarch64-apple-darwin x86_64-apple-darwin`
- Check Xcode installation and command line tools

### Checksum Mismatch
- Recompute checksum: `swift package compute-checksum bindings/swift/OrangeSDKFFI.xcframework.zip`
- Update Package.swift with new checksum
- Ensure the uploaded file matches the computed checksum

### Swift Package Resolution Issues
- Verify the GitHub release URL is accessible
- Check that the release tag matches the URL in Package.swift
- Ensure the zip file is properly uploaded as a release asset
#!/bin/bash
set -e

# Create complete Maven Central package for orange-sdk-android
# This script consolidates Android binding generation, signing, and packaging
#
# Usage:
#   ./scripts/create_android_maven_package.sh [GPG_KEY_ID] [GPG_PASSPHRASE]
#   
# Examples:
#   ./scripts/create_android_maven_package.sh
#   ./scripts/create_android_maven_package.sh YOUR_GPG_KEY_ID
#   ./scripts/create_android_maven_package.sh YOUR_GPG_KEY_ID "your_passphrase"
#
# Environment variables:
#   GPG_KEY_ID       - Override GPG key ID to use for signing
#   GPG_PASSPHRASE   - GPG passphrase (avoid putting in command line)
#
# Security note: For CI/CD, use environment variables instead of command line arguments

VERSION="0.1.0"
ARTIFACT_ID="orange-sdk-android"
GROUP_ID="org.lightningdevkit"
PACKAGE_DIR="maven-central-package"

# Show help if requested
if [[ "$1" == "--help" || "$1" == "-h" ]]; then
    echo "Usage: $0 GPG_KEY_ID [GPG_PASSPHRASE]"
    echo ""
    echo "Creates a complete Maven Central package for orange-sdk-android"
    echo ""
    echo "Arguments:"
    echo "  GPG_KEY_ID      GPG key ID for signing (required)"
    echo "  GPG_PASSPHRASE  GPG passphrase for automated signing (optional)"
    echo ""
    echo "Environment variables:"
    echo "  GPG_KEY_ID      GPG key ID for signing"
    echo "  GPG_PASSPHRASE  GPG passphrase (more secure than command line)"
    echo ""
    echo "Examples:"
    echo "  $0 YOUR_KEY_ID                       # Interactive GPG signing"
    echo "  $0 YOUR_KEY_ID \"your_passphrase\"    # With passphrase"
    echo "  GPG_KEY_ID=\"YOUR_KEY_ID\" $0         # Use environment variable"
    echo "  GPG_KEY_ID=\"YOUR_KEY_ID\" GPG_PASSPHRASE=\"pass\" $0  # Full env vars"
    echo ""
    echo "Find your GPG key ID: gpg --list-secret-keys --keyid-format=long"
    echo "Security: For CI/CD, use environment variables instead of command line arguments"
    exit 0
fi

# Support both command line arguments and environment variables
GPG_KEY_ID="${GPG_KEY_ID:-${1}}"
GPG_PASSPHRASE="${GPG_PASSPHRASE:-${2:-}}"

# Check if GPG key ID is provided
if [[ -z "$GPG_KEY_ID" ]]; then
    echo "Error: GPG key ID is required"
    echo "Usage: $0 GPG_KEY_ID [GPG_PASSPHRASE]"
    echo "   or: GPG_KEY_ID=your_key_id $0"
    exit 1
fi

echo "=== Orange SDK Android Maven Central Package Generator ==="
echo "Creating package: ${GROUP_ID}:${ARTIFACT_ID}:${VERSION}"
echo "Using GPG key: $GPG_KEY_ID"
if [[ -n "$GPG_PASSPHRASE" ]]; then
    echo "Using provided GPG passphrase"
else
    echo "No GPG passphrase provided (will use GPG agent or prompt)"
fi
echo ""

# Check prerequisites
if ! command -v gpg &> /dev/null; then
    echo "Error: GPG is required for signing artifacts"
    exit 1
fi

if [[ ! -f "bindings/kotlin/orange-sdk-android/lib/build/outputs/aar/lib-release.aar" ]]; then
    echo "Error: Android AAR not found. Run ./scripts/uniffi_bindgen_generate_kotlin_android.sh first"
    echo "Then build with: cd bindings/kotlin/orange-sdk-android && ANDROID_SDK_ROOT=\"~/Android/Sdk\" ./gradlew build"
    exit 1
fi

echo "=== Step 0: Generate POM file using Gradle ==="
cd "bindings/kotlin/orange-sdk-android"
if [[ -n "$ANDROID_SDK_ROOT" ]]; then
    echo "Using ANDROID_SDK_ROOT: $ANDROID_SDK_ROOT"
    ANDROID_SDK_ROOT="$ANDROID_SDK_ROOT" ./gradlew generatePomFileForMavenPublication
else
    echo "ANDROID_SDK_ROOT not set, trying default location..."
    ANDROID_SDK_ROOT="~/Android/Sdk" ./gradlew generatePomFileForMavenPublication
fi
cd ../../../
echo "✓ Generated POM file using Gradle"

echo "=== Step 1: Creating Maven Central directory structure ==="
rm -rf "$PACKAGE_DIR"
mkdir -p "$PACKAGE_DIR"
cd "$PACKAGE_DIR"

# Source paths
AAR_DIR="../bindings/kotlin/orange-sdk-android/lib/build/outputs/aar"
SRC_DIR="../bindings/kotlin/orange-sdk-android/lib/src/main/kotlin"

echo "=== Step 2: Creating properly named artifacts ==="

# 1. Main AAR (rename from lib-release.aar)
cp "$AAR_DIR/lib-release.aar" "${ARTIFACT_ID}-${VERSION}.aar"
echo "✓ Created: ${ARTIFACT_ID}-${VERSION}.aar ($(du -h ${ARTIFACT_ID}-${VERSION}.aar | cut -f1))"

# 2. Sources JAR (create from Kotlin source)
echo "Creating sources JAR..."
CURRENT_DIR=$(pwd)
cd "../bindings/kotlin/orange-sdk-android/lib/src/main/kotlin"
zip -r "$CURRENT_DIR/${ARTIFACT_ID}-${VERSION}-sources.jar" org/ > /dev/null
cd "$CURRENT_DIR"
echo "✓ Created: ${ARTIFACT_ID}-${VERSION}-sources.jar ($(du -h ${ARTIFACT_ID}-${VERSION}-sources.jar | cut -f1))"

# 3. Javadoc JAR (empty but required by Maven Central)
echo "Creating javadoc JAR..."
mkdir -p temp-javadoc/META-INF
echo "Manifest-Version: 1.0" > temp-javadoc/META-INF/MANIFEST.MF
cd temp-javadoc
zip -r "../${ARTIFACT_ID}-${VERSION}-javadoc.jar" . > /dev/null
cd ..
rm -rf temp-javadoc
echo "✓ Created: ${ARTIFACT_ID}-${VERSION}-javadoc.jar ($(du -h ${ARTIFACT_ID}-${VERSION}-javadoc.jar | cut -f1))"

# 4. POM file (extract from Gradle-generated publications)
echo "Extracting POM from Gradle build..."
GRADLE_POM="../bindings/kotlin/orange-sdk-android/lib/build/publications/maven/pom-default.xml"
if [[ -f "$GRADLE_POM" ]]; then
    cp "$GRADLE_POM" "${ARTIFACT_ID}-${VERSION}.pom"
    echo "✓ Copied POM from Gradle build: ${ARTIFACT_ID}-${VERSION}.pom"
else
    echo "Warning: Gradle-generated POM not found at $GRADLE_POM"
    echo "Make sure you've run 'gradle generatePomFileForMavenPublication' first"
    echo "Creating basic POM as fallback..."
    
    # Fallback: create basic POM
    cat > "${ARTIFACT_ID}-${VERSION}.pom" << 'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<project xsi:schemaLocation="http://maven.apache.org/POM/4.0.0 https://maven.apache.org/xsd/maven-4.0.0.xsd" xmlns="http://maven.apache.org/POM/4.0.0"
    xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <modelVersion>4.0.0</modelVersion>
  <groupId>org.lightningdevkit</groupId>
  <artifactId>orange-sdk-android</artifactId>
  <version>0.1.0</version>
  <packaging>aar</packaging>
  <name>Orange SDK Android</name>
  <description>Android bindings for Orange SDK - a hybrid Bitcoin wallet library</description>
  <url>https://github.com/lightningdevkit/orange-sdk</url>
</project>
EOF
    echo "✓ Created fallback POM: ${ARTIFACT_ID}-${VERSION}.pom"
fi

echo ""
echo "=== Step 3: Generating checksums for all artifacts ==="

# Function to generate all checksums for a file
generate_checksums() {
    local file="$1"
    md5sum "$file" | cut -d ' ' -f 1 > "$file.md5"
    sha1sum "$file" | cut -d ' ' -f 1 > "$file.sha1"  
    sha256sum "$file" | cut -d ' ' -f 1 > "$file.sha256"
    sha512sum "$file" | cut -d ' ' -f 1 > "$file.sha512"
    echo "  ✓ Generated checksums for: $(basename $file)"
}

# Generate checksums for all main files
for file in *.aar *.jar *.pom; do
    if [[ -f "$file" ]]; then
        generate_checksums "$file"
    fi
done

echo ""
echo "=== Step 4: Signing all files with GPG ==="

# Function to sign a file
sign_file() {
    local file="$1"
    if [[ -f "$file" ]]; then
        if [[ -n "$GPG_PASSPHRASE" ]]; then
            # Use passphrase in batch mode
            echo "$GPG_PASSPHRASE" | gpg --batch --yes --pinentry-mode loopback --passphrase-fd 0 \
                --armor --detach-sign --default-key "$GPG_KEY_ID" "$file" 2>/dev/null
        else
            # Use GPG agent or prompt for passphrase
            gpg --armor --detach-sign --default-key "$GPG_KEY_ID" "$file" 2>/dev/null
        fi
        echo "  ✓ Signed: $(basename $file)"
    fi
}

# Sign all files (main files + checksums)
for file in *; do
    if [[ -f "$file" && "$file" != *.asc ]]; then
        sign_file "$file"
    fi
done

echo ""
echo "=== Step 5: Creating final Maven Central package ==="

# Create final ZIP
zip -r "${ARTIFACT_ID}-${VERSION}-maven-central.zip" * > /dev/null
ZIP_SIZE=$(du -h "${ARTIFACT_ID}-${VERSION}-maven-central.zip" | cut -f1)

echo ""
echo "=== Maven Central Package Complete! ==="
echo "📦 Package: ${ARTIFACT_ID}-${VERSION}-maven-central.zip (${ZIP_SIZE})"
echo "📍 Location: $(pwd)"
echo "📊 Total files: $(ls -1 | grep -v "maven-central.zip" | wc -l)"
echo ""
echo "📋 Package contents:"
echo "   • ${ARTIFACT_ID}-${VERSION}.aar - Main Android library"
echo "   • ${ARTIFACT_ID}-${VERSION}-sources.jar - Source code"
echo "   • ${ARTIFACT_ID}-${VERSION}-javadoc.jar - Documentation"  
echo "   • ${ARTIFACT_ID}-${VERSION}.pom - Maven metadata"
echo "   • All checksums (.md5, .sha1, .sha256, .sha512) for each file"
echo "   • All GPG signatures (.asc) for each file"
echo ""
echo "🚀 Ready for Maven Central upload!"
echo "   Group ID: ${GROUP_ID}"
echo "   Artifact ID: ${ARTIFACT_ID}"
echo "   Version: ${VERSION}"
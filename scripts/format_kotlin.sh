#!/bin/bash
ORANGE_SDK_ANDROID_DIR="bindings/kotlin/orange-sdk-android"
ORANGE_SDK_JVM_DIR="bindings/kotlin/orange-sdk-jvm"

# Run ktlintFormat in orange-sdk-android
(
  cd $ORANGE_SDK_ANDROID_DIR || exit 1
  ./gradlew ktlintFormat
)

# Run ktlintFormat in orange-sdk-jvm
(
  cd $ORANGE_SDK_JVM_DIR || exit 1
  ./gradlew ktlintFormat
)

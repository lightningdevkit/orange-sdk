ditto -c -k --sequesterRsrc --keepParent ./bindings/swift/OrangeSDKFFI.xcframework ./bindings/swift/OrangeSDKFFI.xcframework.zip || exit 1
CHECKSUM=`swift package compute-checksum ./bindings/swift/OrangeSDKFFI.xcframework.zip` || exit 1
echo "New checksum: $CHECKSUM" || exit 1
python3 ./scripts/swift_update_package_checksum.py --checksum "${CHECKSUM}" || exit 1

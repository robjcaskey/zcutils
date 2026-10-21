#!/usr/bin/env bash
# Build a development bundle only. Does not activate a driver or change security.
set -euo pipefail
cd "$(dirname "$0")/.."
sdk=$(xcrun --sdk driverkit --show-sdk-path)
arch=${DRIVERKIT_ARCH:-$(uname -m)}
out=${DRIVERKIT_OUT:-target/macos-driverkit}
cache=${DRIVERKIT_CACHE:-macos/driverkit/generated}
deployment=${DRIVERKIT_DEPLOYMENT_TARGET:-23.0}
python3 scripts/driverkit-interfaces.py verify --sdk "$sdk" --cache "$cache" --arch "$arch" --deployment-target "$deployment"
mkdir -p "$out/generated"
for cls in ZcController ZcUserClient; do
  cp "$cache/$arch/$cls.h" "$cache/$arch/$cls.cpp" "$out/generated/"
done
bundle="$out/ZcStorage.app/Contents/Library/SystemExtensions/org.zcutils.storage.dext"
mkdir -p "$bundle/Contents/MacOS" "$out/ZcStorage.app/Contents/MacOS"
xcrun --sdk driverkit clang++ -std=c++17 -target "$arch-apple-driverkit$deployment" -isysroot "$sdk" \
  -I "$out/generated" -I macos/driverkit -fno-exceptions -fno-rtti \
  macos/driverkit/ZcController.cpp "$out/generated/ZcController.cpp" "$out/generated/ZcUserClient.cpp" \
  -framework DriverKit -framework SCSIControllerDriverKit -o "$bundle/Contents/MacOS/org.zcutils.storage"
cp macos/driverkit/Info.plist "$bundle/Contents/Info.plist"
xcrun swiftc macos/driverkit/Activate.swift -o "$out/ZcStorage.app/Contents/MacOS/ZcStorage" -framework SystemExtensions
cp macos/driverkit/AppInfo.plist "$out/ZcStorage.app/Contents/Info.plist"
# Ad-hoc artifacts exercise compilation, not production signing or installation.
codesign --force --sign - --entitlements macos/driverkit/Driver.entitlements "$bundle"
codesign --force --sign - --entitlements macos/driverkit/App.entitlements "$out/ZcStorage.app"
printf 'Development bundle: %s/ZcStorage.app\n' "$out"

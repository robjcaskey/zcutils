#!/usr/bin/env bash
# Build a development bundle only. Does not activate a driver or change security.
set -euo pipefail
cd "$(dirname "$0")/.."
sdk=$(xcrun --sdk driverkit --show-sdk-path)
arch=${DRIVERKIT_ARCH:-$(uname -m)}
out=${DRIVERKIT_OUT:-target/macos-driverkit}
mkdir -p "$out/generated"
for cls in ZcController ZcUserClient; do
  xcrun iig --def "macos/driverkit/$cls.iig" --header "$out/generated/$cls.h" --impl "$out/generated/$cls.cpp" --deployment-target 23.0 --log "$out/iig.log" -- -isysroot "$sdk" -iframework "$sdk/System/DriverKit/System/Library/Frameworks" -isystem "$sdk/System/DriverKit/usr/include" -x c++ -std=c++17 -D__IIG=1 -DPLATFORM_DriverKit=1 -I macos/driverkit || { cat "$out/iig.log"; exit 1; }
 done
bundle="$out/ZcStorage.app/Contents/Library/SystemExtensions/org.zcutils.storage.dext"
mkdir -p "$bundle/Contents/MacOS" "$out/ZcStorage.app/Contents/MacOS"
xcrun --sdk driverkit clang++ -std=c++17 -target "$arch-apple-driverkit23.0" -isysroot "$sdk" \
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

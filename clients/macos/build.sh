#!/bin/zsh
# Builds "build/Voxl Viewer.app".
#
# Signs with the "Voxl Viewer Development" certificate when its keychain exists, so
# macOS keeps the app's Accessibility and Keychain permissions across rebuilds (an
# ad-hoc signature changes every build, and macOS then forgets them).
set -euo pipefail
cd "${0:A:h}"

swift build -c release
bin="$(swift build -c release --show-bin-path)/VoxlViewer"

app="build/Voxl Viewer.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin" "$app/Contents/MacOS/VoxlViewer"
cp Info.plist "$app/Contents/Info.plist"
[[ -f Resources/AppIcon.icns ]] && cp Resources/AppIcon.icns "$app/Contents/Resources/"

kc=~/Library/Keychains/voxl-viewer-dev.keychain-db
if [[ -f $kc ]]; then
  security unlock-keychain -p voxl-viewer-dev "$kc"
  codesign --force --keychain "$kc" --sign "Voxl Viewer Development" "$app" 2>&1 | grep -v "replacing existing" || true
else
  codesign --force --sign - "$app" >/dev/null
fi

echo "Built $PWD/$app"

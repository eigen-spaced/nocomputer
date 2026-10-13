#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."

swift build --configuration debug --cache-path "$PWD/.build/cache"
binary_dir="$(swift build --configuration debug --show-bin-path --cache-path "$PWD/.build/cache")"
app_path="$PWD/build/NoComputer Diagnostics.app"
mkdir -p "$app_path/Contents/MacOS"
cp resources/Info.plist "$app_path/Contents/Info.plist"
cp "$binary_dir/NoComputerDiagnostics" "$app_path/Contents/MacOS/NoComputerDiagnostics"
# Local ad-hoc signing is enough to build/run; TCC identity stability must be
# tested across rebuilds. A real development certificate can be supplied later.
signing_identity="${NOCOMPUTER_SIGN_IDENTITY:--}"
codesign --force --sign "$signing_identity" "$app_path"
codesign --verify --strict "$app_path"
if [ "$signing_identity" = "-" ]; then
    printf '%s\n' 'Ad-hoc signing: changed builds may invalidate Accessibility/Screen Recording grants even if Settings shows them enabled.'
    printf '%s\n' 'For stable development signing, supply an existing certificate via NOCOMPUTER_SIGN_IDENTITY. No privacy settings have been reset.'
fi
printf 'Built %s\n' "$app_path"

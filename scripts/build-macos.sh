#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."

swift build --configuration debug --cache-path "$PWD/.build/cache"
binary_dir="$(swift build --configuration debug --show-bin-path --cache-path "$PWD/.build/cache")"
app_path="$PWD/build/NoComputer Diagnostics.app"
mkdir -p "$app_path/Contents/MacOS"
cp resources/Info.plist "$app_path/Contents/Info.plist"
# The window's page (docs/host-panel.md); codesign seals it with the bundle.
rm -rf "$app_path/Contents/Resources/panel"
mkdir -p "$app_path/Contents/Resources/panel"
cp panel/index.html panel/panel.js panel/panel.css "$app_path/Contents/Resources/panel/"
cp "$binary_dir/NoComputerDiagnostics" "$app_path/Contents/MacOS/NoComputerDiagnostics"
# A stable certificate keeps the designated requirement (and so TCC/Keychain
# grants) unchanged across rebuilds; ad-hoc signing pins each binary's cdhash.
# Prefer NOCOMPUTER_SIGN_IDENTITY, then the local self-signed development
# identity described in docs/macos-feasibility.md, then ad-hoc.
default_identity="nocomputer Local Development"
if [ -n "${NOCOMPUTER_SIGN_IDENTITY:-}" ]; then
    signing_identity="$NOCOMPUTER_SIGN_IDENTITY"
elif security find-identity -p codesigning | grep -q "\"$default_identity\""; then
    signing_identity="$default_identity"
else
    signing_identity="-"
fi
# Remote access runs the Rust host as a child of the app. Nested code must be
# signed before the bundle; its own identifier keeps its Keychain item separate.
host_manifest="$PWD/crates/host/Cargo.toml"
if [ -f "$host_manifest" ]; then
    cargo build --release --manifest-path "$host_manifest" --bin nocomputer-host
    host_target="$(cargo metadata --format-version 1 --no-deps --manifest-path "$host_manifest" \
        | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')"
    cp "$host_target/release/nocomputer-host" "$app_path/Contents/MacOS/nocomputer-host"
    codesign --force --sign "$signing_identity" --identifier dev.nocomputer.host \
        "$app_path/Contents/MacOS/nocomputer-host"
else
    rm -f "$app_path/Contents/MacOS/nocomputer-host"
    printf '%s\n' 'crates/host not found: built without nocomputer-host; Remote access will be unavailable.'
fi
codesign --force --sign "$signing_identity" "$app_path"
codesign --verify --strict "$app_path"
if [ "$signing_identity" = "-" ]; then
    printf '%s\n' 'Ad-hoc signing: changed builds may invalidate Accessibility/Screen Recording grants even if Settings shows them enabled.'
    printf '%s\n' 'For stable development signing, create the local identity (docs/macos-feasibility.md) or set NOCOMPUTER_SIGN_IDENTITY. No privacy settings have been reset.'
fi
printf 'Built %s\n' "$app_path"

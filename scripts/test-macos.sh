#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."

developer_path="$(xcode-select -p)"
testing_frameworks="$developer_path/Library/Developer/Frameworks"
testing_libraries="$developer_path/Library/Developer/usr/lib"
options=(--disable-xctest --enable-swift-testing --cache-path "$PWD/.build/cache")
if [ -d "$testing_frameworks/Testing.framework" ]; then
    # Command Line Tools ship Swift Testing but SwiftPM does not automatically
    # add these framework/runtime search paths on the current installation.
    options+=(-Xswiftc -F -Xswiftc "$testing_frameworks"
              -Xlinker -F -Xlinker "$testing_frameworks"
              -Xlinker -rpath -Xlinker "$testing_frameworks"
              -Xlinker -rpath -Xlinker "$testing_libraries")
fi
swift test "${options[@]}"

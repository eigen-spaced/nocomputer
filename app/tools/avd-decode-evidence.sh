#!/bin/bash
# macOS hardware-decode evidence for the WebCodecs viewer.
# Lists AppleAVD (Apple Silicon fixed-function video decoder) user clients before,
# during and after visible playback. Pass: a client owned by the viewer's own
# com.apple.WebKit.GPU process (absent before launch) appears during playback.
# WKWebView ignores hardwareAcceleration, so a --software run is NOT a software control.
# Usage: tools/avd-decode-evidence.sh <viewer-binary> <fixture.h264> [--require-hardware|--software]
set -u
avd() { ioreg -w0 -r -c AppleAVD -l | grep -o '"IOUserClientCreator" = "[^"]*"' | sed 's/.*= //' | sort | tr '\n' ' '; }
echo "before: $(avd)"
"$1" ${3:+"$3"} "$2" & app=$!
echo "viewer pid $app"
for i in $(seq 1 16); do
  sleep 0.25
  echo "t=$((i * 250))ms: $(avd)"
  if [ "$i" -eq 6 ]; then
    for p in $(ioreg -w0 -r -c AppleAVD -l | grep -o 'pid [0-9]*' | cut -d' ' -f2 | sort -u); do
      ps -o pid=,lstart=,command= -p "$p"
    done
  fi
done
kill "$app"; wait "$app" 2>/dev/null
sleep 1
echo "after: $(avd)"

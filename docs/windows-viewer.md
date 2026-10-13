# Offline Windows viewer PoC

This is a local-file decoder/rendering experiment, not a networked viewer. It accepts a single regular `.h264` file of at most 8 MiB, forces the H.264 demuxer and permits only FFmpeg's local-file protocol. Use the generated synthetic fixture, not sensitive desktop recordings or untrusted media. No networking, pairing, input injection, clipboard, or screen recording is implemented.

## Implementation choice

C++17 + FFmpeg + SDL3 is a provisional small prototype, not a permanent Windows-language/renderer decision. Compared with a Windows-only Media Foundation pipeline, this lets the same file-decode and presentation code be built and checked on the Mac before handing it to Windows. Windows hardware decoding uses FFmpeg's D3D11VA device/format negotiation; a frame counts as hardware only when received in the negotiated hardware pixel format. See [FFmpeg's hardware-decode example](https://ffmpeg.org/doxygen/8.1/hw_decode_8c-example.html).

Rendering is deliberately simple: hardware frame → CPU transfer → range/matrix-aware BGRA conversion → SDL streaming texture → window. It is **not zero-copy** and is not the final latency path. The converter honours decoded video range and BT.709/BT.2020 matrix metadata; HDR/ICC management and colour tuning are deferred. SDL chooses its renderer and the summary reports its name; do not assume a particular Direct3D renderer was selected. [SDL's texture upload](https://wiki.libsdl.org/SDL3/SDL_UpdateTexture) and [renderer creation](https://wiki.libsdl.org/SDL3/SDL_CreateRenderer) define the presentation boundary.

Processing is synchronous: one packet, one decoded frame, one transferred frame, and one reusable BGRA image/texture; no application video queue. Limits: 1 MiB per demuxed packet, 10,000 decoded frames, dimensions at most 8192 per side and 16 megapixels. Decoder internals may retain reference frames. These are diagnostic limits, not a safe network-media protocol. Playback is paced at 30 Hz for the fixture, holds its last frame at EOF, and never changes monitor resolution. The timing summary totals decoder send/receive call wall time only; it is neither end-to-end latency nor a comparison directly interchangeable with VideoToolbox's callback metric.

## Build on Windows

Prerequisites: Windows 11 x64, Visual Studio C++ build tools (Desktop development with C++ and a Windows SDK), CMake 3.24+, Git, and a bootstrapped vcpkg checkout. None has been installed or verified on your Windows machine by this project. The script does not install compilers or change security/system settings. Dependency builds can take time and disk space.

Copy this project to Windows, plus `build/synthetic-srgb.h264` from the Mac. The fixture is a generated 320×240 gradient with 30 frames, not a screen recording. Build artifacts are ignored by Git, so transferring source alone will not include it. It can be regenerated on the Mac with the documented fixture test in [macOS feasibility](macos-feasibility.md).

The viewer's `vcpkg.json` pins a registry baseline and requests only FFmpeg avcodec/avformat/swscale plus SDL3, without opting into GPL/nonfree codec features. This is not a project license selection or redistribution approval; dependency/patent obligations need review before distribution. See [vcpkg manifest mode](https://learn.microsoft.com/en-us/vcpkg/concepts/manifest-mode).

From PowerShell in the project root (adjust the existing vcpkg path):

```powershell
./scripts/build-windows.ps1 -VcpkgRoot C:/dev/vcpkg -FixturePath ./build/synthetic-srgb.h264
./build/windows-viewer/Release/nocomputer-viewer.exe --require-hardware --check ./build/synthetic-srgb.h264
./build/windows-viewer/Release/nocomputer-viewer.exe --require-hardware ./build/synthetic-srgb.h264
```

The build uses CMake's default Visual Studio generator with x64. Supply `-Generator "Visual Studio 17 2022"` if necessary for an installed version. Do not use a Ninja generator with this script's `-A x64` argument. If PowerShell execution policy prevents running the script, run its CMake commands interactively; do not disable system execution-policy protections just for this test.

`--require-hardware` must succeed and report `hardwareFrames:30` to establish D3D11 decoding. A created device is not evidence. Default mode attempts D3D11 on Windows and explicitly reports software fallback if the device/configuration is unavailable. If hardware format negotiation or decoding fails later, it fails with an error rather than silently replaying. Test `--software` separately to establish a usable fallback and report the hardware error. Dependencies that lack D3D11 support will fail the required-hardware check.

DLLs should be placed alongside the executable by vcpkg's CMake integration. If Windows reports a missing DLL, preserve that error and check build output; do not download random standalone DLLs.

## Manual Windows checklist

- Run required-hardware `--check`: expect 30 frames, 320×240, `hardwareFrames:30`, completed true, and no decode errors. Save the printed summary and build/driver details; it contains no pixel/input/clipboard data.
- Open the fixture visibly. Inspect gradient/channel order and update while playing. A one-second synthetic gradient does not establish desktop text quality.
- Resize the window: fit mode preserves aspect ratio with black letterboxing. Press **N** for one-video-pixel-per-renderer-output-pixel mode (may look small). Arrow keys pan an oversized native view; **R** resets pan.
- Press **F** to enter/exit fullscreen and **Esc** to exit. Confirm the monitor resolution remains unchanged and the last frame remains visible at EOF.
- Run `--software --check` and visible software playback too. Record the actual renderer, GPU/driver, Windows build, dependencies and both summaries. Mac-only tests cannot validate this checklist.

All keys affect the local viewer only; no keys or mouse events are forwarded anywhere.

## Mac development checks

Use already installed FFmpeg/SDL3 development packages; no installation is performed by these commands:

```sh
cmake -S viewer -B build/viewer -DCMAKE_BUILD_TYPE=Debug -DNOCOMPUTER_FIXTURE_PATH="$PWD/build/synthetic-srgb.h264"
cmake --build build/viewer --parallel
ctest --test-dir build/viewer --output-on-failure
build/viewer/nocomputer-viewer --software build/synthetic-srgb.h264
```

CTest covers geometry, rejection of URLs/malformed media, 30-frame software decode, dummy-renderer texture upload, and rejection of Windows-required hardware mode on non-Windows systems. Dummy-renderer checks do not prove actual screen presentation. A Windows build/runtime check remains mandatory before the prototype is called Windows-verified.

On 2026-10-02 all six checks passed on the M3 Mac (AppleClang 21.0.0, FFmpeg 9.0.2, SDL3 3.4.16). A separate `--software --smoke` check with the real Metal renderer also decoded/uploaded all 30 synthetic frames without errors. It uses a hidden window and does not constitute a subjective visible-screen review. Windows compilation, D3D11 hardware decoding and visible presentation remain untested.

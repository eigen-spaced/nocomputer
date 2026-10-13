# nocomputer

A personal remote desktop project for terminal and browser use, initially from Windows to an existing Mac desktop. Future development should support the reverse direction.

The goal is source you can inspect, build, and maintain, with no dependence on a particular VPN provider. Tailscale currently supplies network reachability.

The macOS diagnostic app tests local screen capture, input, and optional hardware H.264 encoding/decoding with an in-memory visual preview. It is not yet a networked remote desktop. No license has been selected.

An [offline Windows viewer PoC](docs/windows-viewer.md) now lives in `viewer/`: C++/FFmpeg/SDL3 local-fixture playback, fit/native pixels, fullscreen, and an unverified Windows D3D11 decoding path. Mac build/decode/render smoke checks pass; Windows build/runtime validation is still required. Build instructions are in that checklist and `scripts/build-windows.ps1`.

Build from this folder:

```sh
bash scripts/build-macos.sh
open "build/NoComputer Diagnostics.app"
```

See the [macOS feasibility checklist](docs/macos-feasibility.md) before testing permissions, screen lock, or display sleep. The app never captures or injects input automatically on launch.

- [Development plan](docs/development-plan.md): scope, setup, feasibility checks, and milestones.
- [Agent instructions](AGENTS.md): constraints for implementation and verification.

Display wake followed by lock-screen capture and basic mouse/keyboard injection is demonstrated on the target Mac, as is live desktop H.264 encoding/decoding. The user accepted the improved 2560-wide decoded text as sufficient for a PoC; colour tuning is deferred. Continuous capture while the panel sleeps is not supported by the tested path. Actual remote unlock, system wake, and Windows decoding/rendering remain unverified. See the development plan for evidence and remaining checks.

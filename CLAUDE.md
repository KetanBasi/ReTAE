# CLAUDE.md — Architectural Laws & Session Guide

## Project
Low-latency LAN audio router/mixer that **replaces VoiceMeeter** and **speaks VBAN** for interop with VoiceMeeter on other machines. Rust everywhere: `egui` (desktop), Makepad (Android), plus a minimal Java shim on Android (Service + consent Activity only). Targets: Windows, Linux (PipeWire), Android 10+. Budget: ≤ 70 ms end-to-end (wired ~15–30 ms, Wi-Fi ~25–55 ms). See `PLAN.md` (v2 supersedes any earlier plan). `crates/audio-types/src/lib.rs` is the authoritative contract: extend it, don't rewrite it.

## Owner conventions (always)
- Comment highlighter: `// * note`, `// ? why/hint`, `// !! danger`, paired `// SECTION: <name>` … `// !SECTION: <name>`.
- Do all file create/edit/delete with the file tools, never shell redirection or heredocs.
- Python helper scripts run with `uv run`, not `python -m`.
- No JavaScript/TypeScript anywhere. Java only inside `crates/android-app/resources/android/java/`, and each Java method is a thin forward to a Rust `native` function.
- Owner is new to Rust: explain non-obvious ownership/lifetime/unsafe choices in a `// ?` comment.

## Laws (never violate)

### 1. RT thread (`audio-engine::process_block` and anything it calls)
- No heap alloc/free, no locks, no syscalls (no I/O, no logging, no `send_to`, no thread spawn).
- Input = `rtrb` consumers + `EngineCommand` queue. `EngineCommand` is `Copy`, ≤ 128 bytes, and **never carries device IDs, sockets or configs**.
- Devices/sockets are opened by the control thread, wired to a slot in the **pre-allocated port-ring pool**, then attached via `Attach*{slot}`. Rings are reused, never freed.
- `assert_no_alloc` wraps `process_block` in debug/test builds.

### 2. Clock domains
- One master device clocks the mixer. **Every** other endpoint (extra HW device, LAN stream, Android playback capture) passes through FIFO → drift resampler → mixer.
- Drift controller: EMA fill vs target ms → PI → ratio, clamp ±1000 ppm, slew ≤ 10 ppm/block. Resampler adds < 0.1 ms latency (cubic).

### 3. Network
- VBAN header is encoded/decoded explicitly (LE) by `audio-types::VbanAudioHeader`. **No `#[repr(packed)]` structs.**
- Frames/packet ≤ `vban_max_frames(ch, bps)`. Default 128.
- One socket per local port, demuxed by `(src_ip, stream_name)`.
- RX: blocking recv with timeout. **No busy-polled non-blocking sockets.** TX woken by futex or 1 ms tick.
- QoS: `IP_TOS` on Linux/Android, qWAVE on Windows (IP_TOS is ignored there). DSCP configurable.
- Unicast only for audio. Broadcast/multicast only for discovery.
- Opus is only in `VbanCodec::User` with the `OPUS_USER_TAG` prefix, behind the `opus` cargo feature. Without it the build is PCM-only and must compile with no C toolchain.

### 4. Jitter buffer
- Reorder window (4 slots, by `frame_counter`) → in-order `rtrb` FIFO whose fill = jitter depth.
- Target = `NetworkBufferPreset::effective_target_ms(packet_ms)`. Underrun → fade out + rebuffer. Overrun → crossfade-drop.

### 5. UI
- UIs are thin views over `app-core::ControlApi`. They never touch engine internals.
- Repaint: UI closed/minimized → 0. Unfocused or silent > 500 ms → `silent_fps`. Active → `active_fps`. Never unconditional `request_repaint()`.
- No OS popup windows. Use inline inspectors.

### 6. Android
- Mic/output = AAudio via `ndk`. App audio = Java `AudioRecord` driven from a Rust thread via `jni` (AAudio cannot do playback capture).
- FGS type `microphone` starts while the UI is visible. `mediaProjection` is added only after consent, and before `getMediaProjection`. Handle `MediaProjection.Callback.onStop` (screen lock on Android 15 QPR1+).
- Hold `WifiLock(WIFI_MODE_FULL_LOW_LATENCY)` + partial WakeLock only while streaming.

## Verification (headless container)
```bash
cargo test --workspace
cargo test -p audio-engine -- --nocapture        # assert_no_alloc + RT bench
cargo test -p net-vban --features opus           # only if libopus/cmake available
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo check -p platform-io --target x86_64-pc-windows-gnu   # needs `rustup target add`; C deps feature-gated
cargo check -p android-app --target aarch64-linux-android   # needs NDK if any cc build is enabled
```
!! `cargo xwin` downloads the MSVC CRT from Microsoft and is usually blocked by the container's network allowlist. Use the `-gnu` target for `check`.
!! Anything in PLAN.md §12 (device latency, Android lifecycle, VoiceMeeter interop) can't be proven in the container. Say so in the session summary instead of claiming it works.

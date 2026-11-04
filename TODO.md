# TODO — blocked on hardware or another machine, or queued

Update in place: delete an item when done and put the result where noted (interop results go to
`docs/interop.md`, S5). `§12` means the PLAN.md checklist. `Nx`/`Dx`/`Ex` refer to `NOTES.md`.

## Needs real hardware or another machine
- [ ] **VoiceMeeter interop (§12.5)**, Wireshark against a real VoiceMeeter:
  - our Int16/Int24/Float32 at 128 and 256 frames play, and its audio decodes here (fixtures: Int16 stereo 256, Float32 8 ch 44);
  - it answers our VBAN-SERVICE ping, and the 676-byte PING0 layout in `service.rs` is right;
  - it accepts our VBAN-TEXT (`0x40`, stream `Command1`); we answer its ping and identify correctly.
- [ ] **Windows, one combined session (S6, all of it compile-checked only)**; the step-by-step list is `docs/windows-testing.md`:
  - `cargo test -p platform-io --test wasapi_live -- --nocapture` (devices, tone through playback and device loopback, exclusive mode, reopen x many) and `cargo test -p net-vban`;
  - real device latency with `IAudioClient3` versus classic shared versus exclusive (the `detail()` string says which one a stream got, W2); is the "fill the buffer" playback policy (about two periods) acceptable or does it need a lower fill target;
  - process loopback (build ≥ 20348, §12.6): include and exclude, a browser (parent-PID root, W4), the channel counts other than 2; what the UI should offer on older builds;
  - VB-CABLE: the name pairing, the input/mic claim (W5), a second cable, VoiceMeeter's devices staying out; the "Virtual Audio Driver" speaker name is a guess (W5);
  - MMCSS "Pro Audio" actually applied to the mixer, net and WASAPI threads (`routerd` prints the outcome; Task Manager / `Get-Process` priority), and `timeBeginPeriod(1)` giving a 1 ms TX tick (`TxStats.max_tick_gap_us`; the Linux dev container saw 3–30 ms, a real host should stay near 1–2 ms; if Windows is worse, use a high-resolution waitable timer; N4);
  - DSCP actually on the wire with qWAVE (Wireshark `ip.dsfield.dscp`, with and without admin or a QoS policy);
  - the tray: engine auto-start, open and close the UI, "Quit tray and engine" (W8); the privacy hint when the microphone is blocked in Windows settings;
  - unplugging a USB device or changing the default device while a stream is open (W6, P10).
- [ ] **Wi-Fi**: real jitter and loss on 5 GHz; confirm the Fast (12 ms) and Medium (24 ms) presets; `WIFI_MODE_FULL_LOW_LATENCY` ping histogram with and without (§12.4).
- [ ] **Discovery**: mDNS between two machines and through an access point (multicast filtering); fallback beacon against VoiceMeeter peers; Android `MulticastLock` (not implemented, J7).
- [ ] **Android (§12.1–3, S8; needs J5 and J6 first, otherwise nothing can be installed)**; the step-by-step list is `docs/android-testing.md`:
  - playback-capture latency (click-loop probe), whether it opens at all, an app with capture disabled, the per-app filter (J2);
  - screen lock vs MediaProjection on Android 15 QPR1+ and older: does the banner say "App audio paused — tap to resume" and does the microphone keep going; consent again after a stop (J3);
  - OEM foreground-service kills with the screen off; the Wi-Fi low-latency lock effect on ping (§12.4);
  - AAudio: which of exclusive/shared and which preset each phone gives (the stream `detail()` says), the mono microphone into a stereo strip, `VoicePerformance` latency, headset/USB/Bluetooth routes (J1);
  - `Process.setThreadPriority` for the net threads (N4/J7); `cargo test -p net-vban` on the device.
- [ ] **Real pipeline latency**: the loopback test measured 24–30 ms (two Fast receive buffers, budget 70 ms) without devices, and `router-cli probe` measures the mixer path (C2) at ~8 ms for an 8 ms target. Device and OS buffers need the real backends (S5/S6, both written, neither measured on hardware); then run `router-cli probe <peer>` between two real machines.
- [ ] **`router-cli probe <ip>` against a real VoiceMeeter** (VBAN-SERVICE round trip, under the §12.5 interop item), and against a ReTAE on another machine over wired and Wi-Fi.
- [ ] **Desktop UI on a real display (S7, U1–U9)**: build `router-ui` on Windows and on Linux with a GPU; run Wayland and X11. Check what no headless test can: that the window looks right (fonts, spacing, a 4K or high-DPI screen), the Rich and Eco frame rates on a real GPU, and that a minimized window really stops repainting (watch CPU/GPU; Wayland gives no minimized hint, U4). Try the fader, the matrix drag, the inline send knob and the narrow layout by hand, and touch input on a touch screen. Run the tray's "Open UI" (W8) and "Start daemon" with a real `routerd`.
- [ ] Re-run the 16×16 bench on the target PC and an Android ARM phone (E2).
- [ ] Run `cargo test -p net-vban --test loopback -- --nocapture` on a real desktop: the mixer test should print no `NOTE: noisy host` and 0 underruns/overruns (N11).

- [ ] **Linux on real hardware (S5)**, with PipeWire and an actual card:
  - the VoiceMeeter ⇄ Linux table in `docs/interop.md` (also §12.5), measured with `router-cli probe <peer>`;
  - real device latency end to end, to decide whether a device-clocked master (P2) is worth building; `output_target_ms` of 8 ms may be tunable lower;
  - rtkit promotion actually succeeds for the mixer and net threads (P7); `routerd` prints the outcome;
  - the ALSA fallback with a real card (P6), including the 16-bit path, and `--backend auto` falling back to it with PipeWire stopped;
  - unplugging a USB device while its stream is open (P10).

## Software, not blocked
- [ ] Show a failed or stalled device stream in the control plane after hot-unplug instead of `Running` (P10, W6; the backends already report it through `AudioStream::detail()`). Follow a changed default device and reopen (W6).
- [ ] Expose `WasapiOptions` (exclusive mode, output target) as a `routerd` flag and a profile field (W9).
- [ ] Graceful "close UI" for the tray instead of `Child::kill` (W8, needs the shutdown command of C5).
- [ ] Packet-phase-independent fill estimate for the drift controller (D1): a step/phase tracker; Optimal only.
- [ ] Rebind sockets after errors (N5); per-interface beacon targets (N6).
- [ ] Ring size versus 8-channel targets (E1).
- [ ] UI: Wi-Fi RSSI and link speed in the LAN inspector (needs a field in the snapshot, U8); a probe view (U8); choose the network buffer preset from measured jitter in Simple mode (U6, with N3).
- [ ] `routerd` graceful shutdown on SIGTERM/Ctrl-C (C5).
- [ ] **Android, finish S8** (decisions for the owner are marked):
  - **Get the Java into the APK (J5).** Decide: ask Makepad upstream, or write the post-build `uv run` script (javac + d8 + add dex + apksigner). Then `cargo makepad android build` must be shown to succeed; it has not been run.
  - **Write the Makepad screen (J6)** over `android-app::view_model`: add `makepad-widgets` pinned to `60abec8b…` (Makepad 2.0 script DSL), call `glue::init` once the Activity exists, `glue::set_ui_visible` from the lifecycle, and draw `Screen` (Simple cards, banner, meters). The sandbox must be allowed to build Makepad's git checkout, or build it yourself.
  - Battery-optimisation prompt and the OEM "don't kill my app" page (PLAN §6.3 step 6); `MulticastLock` while discovery runs (J7); an app picker in the Send card that uses `apps()` and `PlaybackCaptureFilter::OnlyUid`.
  - Recording to `getExternalFilesDir` once the recorder ring has a consumer (J9, C4).
- **S9**: adaptive target (N3), VBAN-TEXT remote control of VoiceMeeter peers (N7), Noise/`snow` pairing, FLAC/Opus recording (drain the recorder ring, C4), patchbay cards and the Record button (shown disabled until then, U7). (S7, the desktop UI, is written; see the display item above.)

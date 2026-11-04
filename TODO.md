# TODO — blocked on hardware or another machine, or queued

Update in place: delete an item when done and put the result where noted (interop results go to
`docs/interop.md`, S5). `§12` means the PLAN.md checklist. `Nx`/`Dx`/`Ex` refer to `NOTES.md`.

## Needs real hardware or another machine
PLAN §12 status: **all six items are still unproven**. 1–3 are under Android, 4 under Wi-Fi, 5 under VoiceMeeter interop, 6 under Windows.
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
- [ ] **S9 on real hardware** (A8, N3, N7, R1–R3, Q1–Q2): a LAN-control pairing between two real machines (wrong code, expired window, restart, Windows firewall prompt for the control port); the adaptive jitter target on real Wi-Fi (does it rise on a bad network and fall back after a clean minute, are 96 ms and the thresholds right); VBAN-TEXT `Solo`, `Pan_x` and A/B routing against a real VoiceMeeter (the `Pan_x` range and the A/B mapping come from the public parameter list, N9), and `PeerText` to it; a long FLAC and WAV take on a real disk (R3), and that the files open in other players; Patchbay look and feel on a real display, a touch screen, and with a screen reader.
- [ ] Re-run the 16×16 bench on the target PC and an Android ARM phone (E2).
- [ ] Run `cargo test -p net-vban --test loopback -- --nocapture` on a real desktop: the mixer test should print no `NOTE: noisy host` and 0 underruns/overruns (N11).

- [ ] **Linux on real hardware (S5)**, with PipeWire and an actual card:
  - the VoiceMeeter ⇄ Linux table in `docs/interop.md` (also §12.5), measured with `router-cli probe <peer>`;
  - real device latency end to end, to decide whether a device-clocked master (P2) is worth building; `output_target_ms` of 8 ms may be tunable lower;
  - rtkit promotion actually succeeds for the mixer and net threads (P7); `routerd` prints the outcome;
  - the ALSA fallback with a real card (P6), including the 16-bit path, and `--backend auto` falling back to it with PipeWire stopped;
  - unplugging a USB device while its stream is open (P10).

## Owner decisions waiting
- **J5**: get our Java into the APK by sending the feature to Makepad upstream, or by a post-build `uv run` script (javac + d8 + extra dex + apksigner)?
- **Opus recording**: is Opus-in-Ogg wanted, given it forces `libopus`/cmake? (WAV and FLAC are done and need no C code.)
- **LAN control scope**: is "paired controller can do everything `ControlApi` can" the intended trust model, or should pairing carry a read-only role? Should LAN control stay off until `--lan-control` is given (today's default)?
- **Pairing UX**: a runtime pairing window needs a CLI verb and a UI button; say which you want first.

## Software, not blocked
- [ ] Show a failed or stalled device stream in the control plane after hot-unplug instead of `Running` (P10, W6; the backends already report it through `AudioStream::detail()`). Follow a changed default device and reopen (W6).
- [ ] Expose `WasapiOptions` (exclusive mode, output target) as a `routerd` flag and a profile field (W9).
- [ ] Graceful "close UI" for the tray instead of `Child::kill` (W8, needs the shutdown command of C5).
- [ ] Packet-phase-independent fill estimate for the drift controller (D1): a step/phase tracker; Optimal only.
- [ ] Rebind sockets after errors (N5); per-interface beacon targets (N6).
- [ ] Ring size versus 8-channel targets (E1).
- [ ] UI: Wi-Fi RSSI and link speed in the LAN inspector (needs a field in the snapshot, U8); a probe view (U8).
- [ ] `routerd` graceful shutdown on SIGTERM/Ctrl-C (C5).
- [ ] **Android, finish S8** (decisions for the owner are marked):
  - **Get the Java into the APK (J5).** Decide: ask Makepad upstream, or write the post-build `uv run` script (javac + d8 + add dex + apksigner). Then `cargo makepad android build` must be shown to succeed; it has not been run.
  - **Write the Makepad screen (J6)** over `android-app::view_model`: add `makepad-widgets` pinned to `60abec8b…` (Makepad 2.0 script DSL), call `glue::init` once the Activity exists, `glue::set_ui_visible` from the lifecycle, and draw `Screen` (Simple cards, banner, meters). The sandbox must be allowed to build Makepad's git checkout, or build it yourself.
  - Battery-optimisation prompt and the OEM "don't kill my app" page (PLAN §6.3 step 6); `MulticastLock` while discovery runs (J7); an app picker in the Send card that uses `apps()` and `PlaybackCaptureFilter::OnlyUid`.
  - Recording to `getExternalFilesDir` (J9): the recorder exists now; wire the directory and add a Record button to the Makepad screen.
  - The sandbox's permission classifier refused `cargo install cargo-makepad` and building the Makepad git checkout (both run external code). Do not retry them from a container; the local agent or the owner runs them.
- [ ] **S9 follow-ups, software only** (what S9 left; all can be done in a container):
  - Opus-in-Ogg recording behind the `opus` feature (R1); FLAC `STREAMINFO` MD5; show `recorder_overruns` in the UI (R2).
  - A runtime way to open a LAN pairing window (CLI verb such as `router-cli lan pair-open`, a UI button) instead of only `routerd --pair` at start (A9); list and revoke paired controllers; auto-reconnect for a `RemoteControl` client after the 300 s idle drop; advertise the control port in mDNS (TXT `ctrl_port`).
  - A `router-cli` verb and a UI box for `Command::PeerText` (N7).
  - Patchbay: reorder bus cards, start a wire from a bus socket, pick a wire over a card (Q2); decide what `smooth_transitions` should do or remove the flag.
  - Android host: pass `DaemonConfig::recordings_dir` from `glue::app_dir(true)` (J9).
- [ ] **Hand-off for the local agent**: run these first on the real machine and report anything that differs from the container (see `docs/windows-testing.md`, `docs/android-testing.md`, `docs/interop.md`):
  ```bash
  cargo test --workspace
  cargo test -p audio-engine -- --nocapture
  cargo clippy --workspace --all-targets -- -D warnings
  cargo fmt --all -- --check
  ```
  Then the hardware items above, then try the new S9 features by hand: `routerd --lan-control --pair` on one machine and `router-cli pair <host> <code>` / `router-cli --lan <host> status` from another; the Record button with `--record-format flac` (open the file in a player, `flac -t`); Simple mode "Receive" over real Wi-Fi with adaptive on; the Patchbay layout in a real window (drag a card, drag a wire, watch Rich vs Eco on a real GPU).

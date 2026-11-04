# TODO — blocked on hardware or another machine, or queued

Update in place: delete an item when done and put the result where noted (interop results go to
`docs/interop.md`, S5). `§12` means the PLAN.md checklist. `Nx`/`Dx`/`Ex` refer to `NOTES.md`.

## Needs real hardware or another machine
- [ ] **VoiceMeeter interop (§12.5)**, Wireshark against a real VoiceMeeter:
  - our Int16/Int24/Float32 at 128 and 256 frames play, and its audio decodes here (fixtures: Int16 stereo 256, Float32 8 ch 44);
  - it answers our VBAN-SERVICE ping, and the 676-byte PING0 layout in `service.rs` is right;
  - it accepts our VBAN-TEXT (`0x40`, stream `Command1`); we answer its ping and identify correctly.
- [ ] **Windows**: DSCP actually on the wire with qWAVE (Wireshark `ip.dsfield.dscp`, with and without admin or a QoS policy); 1 ms TX tick (`TxStats.max_tick_gap_us`; the Linux dev container saw 3–30 ms, a real host should stay near 1–2 ms; if Windows is worse, use a high-resolution waitable timer; N4); run `cargo test -p net-vban` on Windows (today only compile-checked). §12.6: build number (process loopback needs ≥ 20348) and VB-CABLE.
- [ ] **Wi-Fi**: real jitter and loss on 5 GHz; confirm the Fast (12 ms) and Medium (24 ms) presets; `WIFI_MODE_FULL_LOW_LATENCY` ping histogram with and without (§12.4).
- [ ] **Discovery**: mDNS between two machines and through an access point (multicast filtering); fallback beacon against VoiceMeeter peers; Android `MulticastLock` (S8).
- [ ] **Android (§12.1–3)**: playback-capture latency (click-loop probe), screen lock vs MediaProjection, OEM foreground-service kills; run `cargo test -p net-vban` on the device.
- [ ] **Real pipeline latency**: the loopback test measured 24–30 ms (two Fast receive buffers, budget 70 ms) without devices, and `router-cli probe` measures the mixer path (C2) at ~8 ms for an 8 ms target. Device and OS buffers need real backends (S5/S6); then run `router-cli probe <peer>` between two real machines.
- [ ] **`router-cli probe <ip>` against a real VoiceMeeter** (VBAN-SERVICE round trip, under the §12.5 interop item), and against a ReTAE on another machine over wired and Wi-Fi.
- [ ] Re-run the 16×16 bench on the target PC and an Android ARM phone (E2).
- [ ] Run `cargo test -p net-vban --test loopback -- --nocapture` on a real desktop: the mixer test should print no `NOTE: noisy host` and 0 underruns/overruns (N11).

- [ ] **Linux on real hardware (S5)**, with PipeWire and an actual card:
  - the VoiceMeeter ⇄ Linux table in `docs/interop.md` (also §12.5), measured with `router-cli probe <peer>`;
  - real device latency end to end, to decide whether a device-clocked master (P2) is worth building; `output_target_ms` of 8 ms may be tunable lower;
  - rtkit promotion actually succeeds for the mixer and net threads (P7); `routerd` prints the outcome;
  - the ALSA fallback with a real card (P6), including the 16-bit path, and `--backend auto` falling back to it with PipeWire stopped;
  - unplugging a USB device while its stream is open (P10).

## Software, not blocked
- [ ] Show a failed or stalled device stream in the control plane after hot-unplug instead of `Running` (P10).
- [ ] Packet-phase-independent fill estimate for the drift controller (D1): a step/phase tracker; Optimal only.
- [ ] Rebind sockets after errors (N5); per-interface beacon targets (N6).
- [ ] Ring size versus 8-channel targets (E1); show rejected commands in the inspector (E4).
- [ ] `routerd` graceful shutdown on SIGTERM/Ctrl-C (C5).

## Planned (PLAN §11)
- **S6/S8**: real backends for Windows and Android (C1); RT priority for the net threads there (N4: MMCSS, `THREAD_PRIORITY_URGENT_AUDIO`); open devices at 48 kHz (A2); a drift resampler on their bus outputs (C3, reuse `ring_io::PlaybackState`).
- **S9**: adaptive target (N3), VBAN-TEXT remote control of VoiceMeeter peers (N7), Noise/`snow` pairing, FLAC/Opus recording (drain the recorder ring, C4), patchbay cards.

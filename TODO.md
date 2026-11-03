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
- [ ] **Real pipeline latency**: the loopback test measured 13.4 ms at Fast (budget 70 ms) without devices; device and OS buffers need `router-cli probe` (S4/S5).
- [ ] Re-run the 16×16 bench on the target PC and an Android ARM phone (E2).
- [ ] Run `cargo test -p net-vban --test loopback -- --nocapture` on a real desktop: the mixer test should print no `NOTE: noisy host` and 0 underruns/overruns (N11).

## Software, not blocked
- [ ] Predictive concealment for the Optimal preset (N1), then tighten the `jitter_sim` thresholds.
- [ ] Packet-phase-independent fill estimate for the drift controller (D1).
- [ ] Rebind sockets after errors (N5); per-interface beacon targets (N6).
- [ ] Ring size versus 8-channel targets (E1); show rejected commands in the inspector (E4).

## Planned (PLAN §11)
- **S4**: wire `RxSocket`, `TxStream`, `Service`, `Discovery` into `routerd`; fill `RxPolicy.allowed` from discovered or paired peers; `router-cli probe` uses `Service::ping` and `Peer.rtt`.
- **S5/S6/S8**: RT priorities for the net threads (N4); backends open devices at 48 kHz (A2).
- **S9**: adaptive target (N3), VBAN-TEXT remote control of VoiceMeeter peers (N7), Noise/`snow` pairing, FLAC/Opus recording, patchbay cards.

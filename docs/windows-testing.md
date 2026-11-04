# Windows test session (S6)

Everything in the Windows backend was written and checked on Linux (`cargo check`/`clippy` for
`x86_64-pc-windows-gnu`, plus unit tests of the pure logic). **None of it has run on Windows.**
This is the checklist for the first run, ideally together with the VoiceMeeter interop table in
`docs/interop.md` and the Android checks, as one hardware session. Write results into the tables
here (date, Windows build, driver names), and move anything that fails into `TODO.md`.

Machine notes to record first:

| Item | Value |
|---|---|
| Windows version and build (`winver`) | |
| Sound device(s), driver (Realtek / USB / ASIO-only?) | |
| VB-CABLE installed? Which packs (A, B, Hi-Fi)? | |
| VoiceMeeter edition and version | |
| Virtual Audio Driver installed? | |

## 1. Build

Needs the MSVC toolchain (`rustup default stable-x86_64-pc-windows-msvc`) and the Windows SDK; no C
dependency is involved in the default build.

```bash
cargo build --release --workspace
cargo test --workspace
```

`cargo test --workspace` should also pass on Windows. Anything that fails here that passes on Linux
is a Windows-only bug: note the test name.

## 2. The WASAPI live tests

```bash
cargo test -p platform-io --test wasapi_live -- --nocapture
```

Each test prints `SKIP:` and passes if the machine has no audio device. The first test prints what
the machine offers; paste it here.

| Check | Expected | Result |
|---|---|---|
| `report_what_the_machine_offers`: build, capabilities, devices, apps | process loopback `true` only on build ≥ 20348; cables listed as virtual in/out | |
| `a_tone_survives_playback_and_device_loopback` | RMS 0.03–0.30, about 1000 Hz | |
| `failed_opens_hand_the_port_back_and_say_why` | clear error text, port returned | |
| `exclusive_mode_plays_when_the_device_allows_it` | plays, or skips with a reason (the device may refuse) | |
| `a_stream_can_be_closed_and_reopened_many_times` | no hang, no leak | |

Also run `cargo test -p net-vban` (qWAVE, the 1 ms timer, sockets are all Windows-specific code).

## 3. The engine by hand

```bash
cargo run --release -p routerd -- --backend wasapi
```

In a second terminal:

```bash
cargo run --release -p router-cli -- devices
cargo run --release -p router-cli -- apps
cargo run --release -p router-cli -- status
```

Check:

* `routerd` prints the real-time outcome: MMCSS "Pro Audio" for the mixer and net threads. Record
  whether it says it worked (N4).
* `devices` lists your outputs, microphones and the cables; the default is marked.
* `apps` is empty with a note below build 20348, otherwise lists programs that have an audio
  session, those playing first. A browser shows once, not once per tab/process (W4).
* Route a microphone to a speaker with `router-cli route` and listen: no clicks, no growing delay.
  Each stream's `detail()` shows whether it got `IAudioClient3`, classic shared or exclusive, and
  the period; record the period and how it feels.

| Case | Path taken (`detail`) | Period (ms) | Heard latency / glitches |
|---|---|---|---|
| Mic → speakers, shared, `IAudioClient3` | | | |
| Mic → speakers, classic shared (set `low_latency` off) | | | |
| Exclusive, if you add the option (W9) | | | |
| USB headset instead of the on-board device | | | |

Playback currently keeps the device buffer full (about two periods, W2). If that is too much delay
with `IAudioClient3`'s minimum period, say so: the next step is a lower fill target.

## 4. Process loopback (§12.6)

Needs build ≥ 20348. On older builds confirm `apps` is empty and that opening a process capture
reports `NotSupported` with the build number, not a silent device capture.

| Case | Result |
|---|---|
| Capture one program (music player) while another program also plays: only the chosen one is heard | |
| A browser with several tabs: one entry, whole browser captured (W4) | |
| ExcludeTree: everything except one program; routed back out to the same device without feedback | |
| Closing the target program while capturing: the stream goes quiet, no hang | |
| Mono and 6/8 channel capture (untested channel counts) | |

## 5. Virtual cables

| Case | Result |
|---|---|
| VB-CABLE listed as one virtual input and one virtual mic (name pairing, W5) | |
| Play a program into `CABLE Input`, record `CABLE Output` as a virtual input in ReTAE: tone arrives | |
| Send a ReTAE mix to a virtual mic (plays into `CABLE Input`); another program records `CABLE Output` | |
| Use the same cable as input and mic at once: the second open says `Busy` (expected) | |
| With CABLE A and B installed: input on one, mic on the other, both work | |
| VoiceMeeter's own devices are not offered as cables | |
| Virtual Audio Driver (if installed): is the name matched? record the exact endpoint name | |

## 6. Network

| Check | Result |
|---|---|
| Wireshark `ip.dsfield.dscp` on our outgoing VBAN with qWAVE, as normal user and as administrator, and with a QoS policy | |
| `TxStats.max_tick_gap_us` near 1–2 ms (`router-cli status`); if much worse use a high-resolution waitable timer (N4) | |
| The VoiceMeeter table in `docs/interop.md`, with this machine as the Windows end | |
| Firewall: the first run asks to allow `routerd` on private networks; UDP 6980 works after allowing | |

## 7. Tray

Build puts `router-tray.exe` next to `routerd.exe`. `router-ui` is still a stub until S7, so only the
engine part and the menu logic can be checked now.

| Check | Result |
|---|---|
| Running `router-tray` with no engine starts `routerd` with no console window | |
| Running it with an engine already up does not start a second one | |
| The icon appears; left click and the menu item "Open ReTAE" run `router-ui` if present, otherwise print a clear message | |
| "Close ReTAE window" ends the UI process and leaves the engine running | |
| "Quit tray" leaves the engine running; "Quit tray and engine" (enabled only when the tray started it) ends it | |

## 8. Failure modes

| Case | Expected | Result |
|---|---|---|
| Unplug a USB sound device while it streams | worker ends, `detail()` says `STOPPED …` (control plane still shows `Running`, W6) | |
| Change the default output in Windows settings | nothing follows it yet (W6) | |
| Microphone access off in Windows privacy settings | open fails with the privacy hint | |
| Another program holds the device in exclusive mode | `Busy` | |
| Sleep and wake the PC with a stream open | note what happens | |

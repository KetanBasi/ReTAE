# Interop and Linux backend results

What has been checked, where, and what is still waiting for real machines. `PLAN.md` §11 S5 and
§12 define the checklist; `TODO.md` holds the hardware-blocked items. Fill the tables in as you
run them and keep dates and versions, because VoiceMeeter and Windows change between releases.

## 1. VoiceMeeter ⇄ Linux checklist (not yet run)

**Status: blocked on hardware.** It needs a Windows machine with VoiceMeeter and a Linux machine
on the same LAN. The development container has neither, so nothing in this section is proven.

The Windows side of ReTAE (S6, `--backend wasapi`) is just as unproven and shares this blocker, so
the same session can also run the ReTAE ⇄ ReTAE and ReTAE ⇄ VoiceMeeter rows with ReTAE as the
Windows endpoint. The Windows-only checklist is in `docs/windows-testing.md`.

Setup:

1. On Windows, in VoiceMeeter's VBAN window, add an incoming stream from the Linux machine's IP
   (name `ReTAE1`) and an outgoing stream to it (name `VM1`).
2. On Linux, start the router with a real backend:
   `cargo run -p routerd --features linux-audio -- --backend auto --discovery`.
3. Wire a source to a LAN output and a LAN input to a sink with `router-cli route` and the UI, or
   load a profile. Use Wireshark (`udp.port == 6980`) to see what is on the wire.
4. For each row, play a 1 kHz tone at the sender and listen/record at the receiver.

| Direction | Format | Frames/packet | Plays correctly | Latency (`router-cli probe <peer>`) | Date / versions | Notes |
|---|---|---|---|---|---|---|
| Linux → VoiceMeeter | Int16 | 128 | | | | |
| Linux → VoiceMeeter | Int16 | 256 | | | | |
| Linux → VoiceMeeter | Float32 | 128 | | | | |
| Linux → VoiceMeeter | Float32 | 256 | | | | |
| VoiceMeeter → Linux | Int16 | 128 | | | | |
| VoiceMeeter → Linux | Int16 | 256 | | | | |
| VoiceMeeter → Linux | Float32 | 128 | | | | |
| VoiceMeeter → Linux | Float32 | 256 | | | | |

Also record (PLAN §12.5):

| Question | Result |
|---|---|
| Does VoiceMeeter answer our VBAN-SERVICE ping (`router-cli probe <windows-ip>`)? | |
| Is the PING0 layout in `net-vban/src/service.rs` accepted (no "unknown" fields in VoiceMeeter's list)? | |
| Do our VBAN-TEXT commands (stream `Command1`) change a VoiceMeeter strip gain? | |
| Do we decode its audio at Int24? | |

Int24 is listed in §12.5 but not in the S5 checklist; try it if the rows above pass.

## 2. What was verified in the development container (Linux, no sound card)

Environment: PipeWire 1.0.5 and WirePlumber running headless, null audio sinks standing in for a
sound card, `pw-cat` as an independent client. This proves the **graph** behaviour of the
backend. It proves nothing about a real card's buffers or latency.

| Check | Result |
|---|---|
| Enumerate sinks and sources with direction, channels and the default | passes (`platform-io/tests/pipewire_live.rs`) |
| A tone played through the engine to a sink arrives intact through that sink's monitor | passes: RMS 0.354 (expected for a 0.5 sine), 1000 Hz, 0 xruns |
| Virtual input (`Audio/Sink`) and virtual mic (`Audio/Source`): another program plays into one and records from the other | passes: RMS 0.354, 1000 Hz |
| Capture one application among two that play to the same sink | passes: 440 Hz heard, the 2 kHz program does not leak in |
| Close and reopen a stream five times, ports returned to the pool each time | passes |
| Failed opens give the port back and name the reason | passes |
| The same through the whole daemon (`routerd/tests/pipewire_e2e.rs`: commands in, tone out of the virtual mic) | passes |
| ALSA fallback: lists devices, refuses virtual and loopback endpoints, returns ports on failure | unit tests pass; no ALSA card to open here |

How to run them (a PipeWire server and WirePlumber must be running; each test prints `SKIP:` and
passes when there is none):

```bash
cargo test -p platform-io --features pipewire,rt --test pipewire_live
cargo test -p routerd --features pipewire,rt --test pipewire_e2e
```

Not proven here, and listed in `TODO.md`: round-trip latency through a real device, real-time
priority through rtkit (the container has no rtkit), behaviour when a USB device is unplugged
while open, ALSA with a real card, and every row of section 1.

## 3. Behaviours worth knowing

* **Virtual mic class.** The virtual mic is an `Audio/Source` node marked `node.virtual`, not
  `Audio/Source/Virtual` as `PLAN.md` first said. On PipeWire 1.0.5 the `…/Virtual` class never
  gets its capture ports (`pw-loopback` shows the same), so WirePlumber cannot link anything to
  it. Programs see it as an ordinary microphone named "ReTAE Virtual Mic N".
* **A session manager is required** (WirePlumber or PipeWire-Media-Session): it links device
  streams to their device and other programs to our virtual nodes. Without one, nothing is
  linked and no audio flows. (Per-application capture creates its own links, but the program
  being captured still needs the session manager to start playing.)
* **Minimum PipeWire is 0.3.49** (Ubuntu 24.04 and newer; 22.04 ships 0.3.48 and is not
  supported).
* **`pw-cat` in PipeWire 1.0 reads and writes through libsndfile**, so tests feed it WAV files.

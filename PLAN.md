# Master Implementation Plan (`PLAN.md`) — v2

**Project:** Cross-platform, low-latency LAN audio router & mixer. A full **VoiceMeeter replacement** (not a companion app) that also **speaks VBAN** so it interoperates with VoiceMeeter on other machines.
**Stack:** Rust everywhere. `egui` on Windows/Linux, **Makepad** on Android, plus a minimal Java shim on Android only where Android requires a Java class.
**Targets (priority):** Windows 10/11, Linux (PipeWire), Android 10+ (API 29+).

> !! **This file fully supersedes PLAN v1.** Do not consult v1. The `audio-types` contract is the file `crates/audio-types/src/lib.rs` (already implemented and tested). Use it verbatim and don't regenerate it from this document.

---

## 0. What changed from v1 (Gemini) and why

| # | v1 said | Problem | v2 |
|---|---|---|---|
| 1 | Android internal audio via **Oboe** `PlaybackCapture` | AAudio/Oboe has **no** playback-capture API. `AudioPlaybackCaptureConfiguration` only plugs into Java `AudioRecord.Builder.setAudioPlaybackCaptureConfig()`, and it needs a `MediaProjection` token plus a `mediaProjection` foreground service. | Rust drives `AudioRecord` over JNI. A tiny Java shim covers only the Service + consent Activity (§6.3). |
| 2 | Drift compensation for **LAN streams only** | Two USB interfaces on one PC drift just like two PCs. VoiceMeeter resamples every non-master device. | **Clock-domain model**: one master device clocks the mixer, and *every* other endpoint (HW or LAN) gets an adaptive resampler (§5). |
| 3 | Jitter buffer = `rtrb` + reorder by `frame_counter` | `rtrb` is a FIFO and can't reorder. | Small reorder window in the RX thread, then an in-order FIFO whose fill level *is* the jitter depth (§5.2). |
| 4 | `Optimal = 5 ms (2 packets)` | 2 packets of 256 frames = 10.7 ms. With 128 frames it's 5.3 ms. Presets ignored packet size. | Presets in ms, clamped to `≥ 2 × packet_ms` (`effective_target_ms`). Added `VerySlow` to match VoiceMeeter's Net Quality list. |
| 5 | Fixed 128/256 frames per packet | VBAN payload ≤ 1436 B. A VoiceMeeter 8-ch bus as Float32 fits **44** frames, not 256. | `vban_max_frames(ch, bps)`, tested. Engine is N-channel (≤ 8), not stereo-only. |
| 6 | `#[repr(C, packed)] VbanHeader` | Taking a reference to a packed field is UB and the derives are fragile. It also assumes a little-endian host. | Plain struct with explicit LE `encode`/`decode` (no extra cost). |
| 7 | `VbanBitDepth` = 3 values | VBAN defines 8 data types and VoiceMeeter can send Int32/Float32/24-bit. | Full `VbanDataType` + `VbanCodec` + `VbanSubProtocol` (Audio/Serial/Text/Service). |
| 8 | `FixedStr` serde | **Didn't compile** (`<S: N: Serializer, const usize>`). Fixed, it **can't load its own JSON** (`invalid type: sequence, expected a borrowed byte array`). Truncating `"éé"` to 3 bytes returned `""`. | Serialized as a string and truncated on char boundaries. TOML+JSON round-trip tested. |
| 9 | `EngineCommand::SetStripSource{device_id}` to RT thread | Opening a device can't happen on the RT thread anyway. Sending device configs through it is pointless and bloats every command. | Control plane opens devices, then sends `AttachStripInput{slot}` into a **pre-allocated port-ring pool** (§7). |
| 10 | "Non-blocking sockets" | A non-blocking socket polled in a loop burns a core. | Blocking `recv` with a 5 ms timeout on the RX thread. TX thread is woken by futex or a 1 ms tick. The RT thread never makes syscalls. |
| 11 | `set_tos(0xB8)` everywhere | **Windows ignores `IP_TOS`**. DSCP 46 maps to WMM **AC_VI**, not AC_VO, on legacy APs that use the 3-bit precedence mapping. | Windows uses qWAVE `QOSAddSocketToFlow(QOSTrafficTypeVoice)`. DSCP is configurable (46 default, 48 option). |
| 12 | `rubato::FastFixedIn` | Pre-1.0 API. rubato 1.x uses `Async::new_poly/new_sinc(..., FixedAsync)`. | Use `Async::new_poly` (cubic: almost no added latency) + `set_resample_ratio_relative`, or a ~60-line cubic-Hermite drift resampler of our own (§5.1). |
| 13 | Process Loopback "Windows 10/11" | Needs **build 20348+**. Win10 22H2 (19045) doesn't have it. | Feature-detect it and fall back to whole-device loopback. |
| 14 | Oboe on Android | `oboe` = C++ build + a crate that's rarely maintained. | AAudio directly through the `ndk` crate (pure-Rust FFI, API 26+). |
| 15 | Mic/app capture in background | Not addressed. Android blocks background mic without a `microphone` FGS. On Android 15 QPR1+ MediaProjection **stops on screen lock**. | Explicit Android lifecycle (§6.3), including the lock-screen limitation. |
| 16 | No Wi-Fi power-save handling | **Probably your 200 ms.** Phone Wi-Fi in power-save holds packets until the next beacon/DTIM. | Android `WifiLock(WIFI_MODE_FULL_LOW_LATENCY)` + guidance for Windows NIC power settings (§4.4). |
| 17 | mDNS-only discovery | Android needs a `MulticastLock`. Many APs filter mDNS across bands. VoiceMeeter doesn't speak mDNS. | mDNS + VBAN SERVICE beacon + manual IP (§4.5). |
| 18 | "Pure-Rust ADPCM fallback if libopus missing" | Adds a third codec nobody else speaks. | Opus behind a cargo feature; with the feature off you get PCM only. |
| 19 | Spring physics, tilt, particles in v1 | Goes against "lightweight" and costs a lot of UI time before audio works. | Moved to a polish phase. Console-strip view + matrix ship first. |
| 20 | UI and engine in one process | When the window is closed, the GPU context (tens of MB) stays resident. Can't reopen a winit loop in the same process. | **Engine daemon + thin UI client** on desktop. Android: engine in the FGS, UI in the Activity, same process (§2). |
| 21 | `ViewportInfo::minimized` as the 0-FPS trigger | Wayland can't report minimized (`None`). | Close-to-tray = UI process exits (true 0 %). Unfocused falls back to silent FPS. |

---

## 1. Goals / non-goals

**Goals:** (1) simple mode + expert mode; (2) any LAN peer, any OS, VoiceMeeter VBAN interop; (3) ≤ 70 ms end-to-end with a configurable network buffer; (4) VoiceMeeter-style strip→bus routing; (5) low CPU (mixer < 1 % of one core at 16×16 @ 48 kHz); (6) per-channel source/sink selection; (7) **Android: capture app audio + mic, send over LAN, record locally, keep running in background.**

**Non-goals for v1:** virtual audio *driver* on Windows (we use an existing cable driver, §6.1), VST/CLAP hosting, VBAN video/serial, iOS/macOS.

---

## 2. Process & thread architecture

```
 DESKTOP                                         ANDROID (single process)
 ┌────────────── routerd (engine daemon) ─────┐  ┌───────── FGS: AudioEngineService (Java, ~80 lines) ──┐
 │ control thread  (opens devices, config,    │  │   └─ JNI → Rust engine (same crates as desktop)      │
 │                  VBAN-TEXT, discovery)     │  │ Activity: Makepad UI ── in-process ControlApi ──┐    │
 │ RT mixer        (runs in master device cb) │  │ Consent Activity (Java, ~40 lines, transparent)   │    │
 │ device threads  (WASAPI/PipeWire/AAudio)   │  └──────────────────────────────────────────────────┘    │
 │ net RX / net TX threads                    │
 │ recorder thread (WAV/FLAC writer)          │
 └──────── local IPC (UDS / named pipe) ──────┘
              │ ControlMsg (postcard) + 30 Hz meter snapshots
 ┌──────── router-ui (egui, optional) ────────┐   ← close it → 0 CPU, 0 GPU memory, audio keeps running
 └────────────────────────────────────────────┘
```

- **`app-core`**: one `ControlApi` (commands, state snapshots, meter snapshots). It runs in-process on Android and over IPC on desktop. Both UIs are thin views over it, so having two UI toolkits costs only the view code.
- The same `ControlApi` can go over LAN later (authenticated with Noise via `snow`, pure Rust). Then the phone UI can drive the PC mixer.
- **Tray**: `tray-icon` lives in a tiny `routerd` tray helper. Clicking it spawns `router-ui`.

---

## 3. Latency budget (realistic, measure every stage)

Engine block = 128 frames = **2.67 ms**. Packet = 128 frames (default) = 375 packets/s.

| Stage | PC→PC wired | Android mic → PC (5 GHz) | Android app audio → PC | Notes |
|---|---|---|---|---|
| Capture | 3–10 ms | 5–20 ms | **20–40 ms?** | Playback capture goes through the audio-policy remote-submix path, not MMAP. **Measure on your phone.** |
| Mixer + DSP | < 0.5 | < 0.5 | < 0.5 | |
| Packetize | 2.7 | 2.7 | 2.7 | 256 frames → 5.3 ms |
| Transit | 0.2–1 | 2–10 (spikes 50+ with power-save) | same | §4.4 |
| Jitter buffer | 6 (Optimal) | 12 (Fast) | 12–24 | |
| Drift resampler | ~0.05 (cubic) | ~0.05 | ~0.05 | sinc len 128 would add 1.3 ms |
| Playback | 3–10 | 3–10 | 3–10 | WASAPI shared default period 10 ms; `IAudioClient3` low-period can go to 128–144 frames if the driver supports it |
| **Total** | **~15–30 ms** | **~25–55 ms** | **~40–90 ms** | App-capture path may go over 70 ms. It's an Android limit, not ours. |

**Bluetooth headphones add 100–250 ms on their own.** If your current 200 ms setup uses BT anywhere, that alone explains it.

### Built-in latency probe (new)
`router-cli probe <peer>`: sends a VBAN-SERVICE ping (RTT and network jitter histogram), then a click-loop test. Device A emits a click to the peer, the peer routes it back, and A cross-correlates the click it recorded. The result is a per-stage breakdown from telemetry, so we tune with numbers, not guesses.

---

## 4. Network transport

### 4.1 VBAN compatibility matrix
| Feature | RX | TX | Notes |
|---|---|---|---|
| Audio PCM Int16/Int24/Float32 | ✅ | ✅ | Int16 default (VoiceMeeter default) |
| Audio PCM Byte8/Int32/Float64 | ✅ | ❌ | Accept, don't produce |
| 10/12-bit packed, VBCA/VBCV codecs | ❌ drop + count | ❌ | |
| 1–8 channels (≤ 256 on wire) | ✅ | ✅ | Downmix/upmix map per strip |
| VBAN-TEXT (`Strip[0].Gain=-6;`) | ✅ subset | ✅ | Remote-control VoiceMeeter *and* be controllable by VBAN remotes |
| VBAN-SERVICE ping/identity | ✅ reply | ✅ | **Check against VoiceMeeter with Wireshark before relying on it** |
| Our Opus in `VBAN_CODEC_USER` | ✅ | ✅ | Payload starts with tag `"O1"`, so other apps' USER payloads get ignored |

Port model: **one UDP socket per local port (default 6980)**, demuxed by `(src_ip, stream_name)`, the same way VoiceMeeter's incoming slots work.

### 4.2 Packet size vs Wi-Fi airtime
| Frames/pkt | Duration | Packets/s | PCM s16 stereo L3 rate |
|---|---|---|---|
| 64 | 1.33 ms | 750 | ~1.9 Mb/s |
| **128 (default)** | 2.67 ms | 375 | ~1.7 Mb/s |
| 256 | 5.33 ms | 187.5 | ~1.6 Mb/s |

On Wi-Fi, per-packet overhead (contention, preamble, ACK ≈ 0.1–0.3 ms) costs more than bytes do. That's why **Opus barely helps on a clean LAN**: 2.5 ms Opus frames mean 400 packets/s, the same packet rate as PCM. Opus cuts throughput about 5× (1.7 → ~0.3 Mb/s @128 kb/s), which matters on congested 2.4 GHz or with many streams. **Expert mode exposes frames/pkt.** Simple mode uses 128 frames on wired and 256 on Wi-Fi when the link is flapping.

Opus notes: `OPUS_APPLICATION_RESTRICTED_LOWDELAY` (CELT-only, 2.5 ms algorithmic delay). **In-band FEC needs SILK, so it isn't available here. Only PLC.** For loss resilience, use optional packet duplication (send each packet twice, 1 packet apart). That's cheap on LAN.

### 4.3 Sockets
- `socket2`: `SO_RCVBUF`/`SO_SNDBUF` 256 KB, `SO_REUSEADDR`, bind once per port.
- RX thread: blocking `recv_from` with a 5 ms timeout. Decode into a stack buffer, then write into the stream's reorder window.
- TX thread: the RT mixer pushes PCM into a per-stream `rtrb`. TX wakes on a futex (`atomic-wait`, a non-blocking wake) or a 1 ms tick, then packetizes and calls `send_to`.
- QoS: Linux/Android `IP_TOS = dscp << 2`. **Windows: qWAVE** `QOSCreateHandle` + `QOSAddSocketToFlow(..., QOSTrafficTypeVoice, QOS_NON_ADAPTIVE_FLOW)`. Setting an explicit DSCP via `QOSSetFlow` needs admin, so document the Group-Policy QoS alternative.
- Thread priority: Windows MMCSS `"Pro Audio"` (`AvSetMmThreadCharacteristicsW`), Linux rtkit via D-Bus, Android `Process.setThreadPriority(THREAD_PRIORITY_URGENT_AUDIO)` via JNI.

### 4.4 Wi-Fi rules (the probable cause of your 200 ms)
1. **Android:** hold `WifiManager.createWifiLock(WIFI_MODE_FULL_LOW_LATENCY)` (API 29+, only honored while the app is foreground/FGS) + `PARTIAL_WAKE_LOCK`, acquired from Rust via JNI while a stream is active.
2. **Windows laptop:** the UI warns when the adapter's power plan is not "Maximum Performance" (read via `powercfg`). Docs link.
3. **Never broadcast/multicast audio.** On Wi-Fi it's sent at the lowest basic rate and buffered until DTIM (100–300 ms). Unicast only. Broadcast is for discovery.
4. Prefer 5 GHz. Show RSSI/link speed in the stream inspector (Android `WifiInfo`, Windows `WlanQueryInterface`).

### 4.5 Discovery & pairing
- `mdns-sd`: `_lanrouter._udp.local.` TXT `{name, os, ver, vban_port, codecs=pcm,opus, ctrl_port}`. Android takes a `MulticastLock` during discovery only.
- Fallback: VBAN-SERVICE beacon broadcast every 2 s on 6980. VoiceMeeter peers show up when they reply to ping.
- Manual IP entry is always available.
- **Security:** VBAN has no auth. Default RX policy is "accept from discovered/paired peers only". "Accept any IP" is an expert toggle.

---

## 5. Clocking, drift, jitter

### 5.1 Clock domains
- The **master domain** is the device the mixer runs in, by default the A1 output device's callback (the same choice VoiceMeeter makes). If no output exists (e.g. phone streaming mic only), the master is the first input device. If neither exists, a timer thread (`timerfd`/`CreateWaitableTimerEx HIGH_RESOLUTION`).
- **Every other endpoint** (extra HW inputs/outputs, every LAN RX stream, Android playback capture) connects through:
  `device/net thread → rtrb FIFO → DriftResampler (in RT mixer) → mixer`
  Outputs work the same way in the other direction.
- **Controller:** EMA of FIFO fill (τ ≈ 1 s) vs target ms, then PI → ratio. Clamp ±1000 ppm (crystals are ±20–100 ppm; 1000 ppm = 1.7 cents, inaudible). Gentle slew (≤ 10 ppm per block).
- **Resampler:** rubato `Async::new_poly(1.0, 1.01, PolynomialDegree::Cubic, ENGINE_BLOCK_FRAMES, ch, FixedAsync::Output)` with `process_into_buffer` (no allocation), **or** our own cubic-Hermite variable-ratio interpolator (~60 lines, zero latency, trivially alloc-free). Pick after the session-2 benchmark.

### 5.2 LAN jitter buffer
1. **Reorder window** (RX thread): 4 packet slots indexed by `frame_counter % 4`. Packets emit in order. A gap that's still missing when the window advances becomes concealment. Counters: lost, late, duplicate.
2. **Play-out FIFO** (`rtrb`, capacity = 4 × target): its fill level *is* the jitter depth the controller locks to.
3. **Underrun:** output ~3 ms of fade-to-zero (or Opus PLC), set `rebuffering`, wait until fill ≥ target, fade in. **Overrun** (> 2 × target): drop the oldest with a crossfade. Never resample hard to catch up.
4. **Adaptive mode (expert, optional):** raise the target when underruns are > 1/min and slowly lower it after 60 s clean. Presets stay the fixed floors.

---

## 6. Platform backends

### 6.1 Windows
- Crates: `wasapi` (HEnquist) + `windows`. **Not cpal**: cpal doesn't expose `IAudioClient3` low-period streams or process loopback.
- Device I/O: event-driven shared mode. `IAudioClient3::InitializeSharedAudioStream` with the minimum period when supported. Exclusive mode as an expert option.
- **Per-app capture:** `AudioClient::new_application_loopback_client(pid, include_tree)` (in `wasapi`). Pass the *parent* PID for multi-process apps (browsers, Discord). `ExcludeTree` with our own PID gives "all system audio except the router", which breaks feedback loops. Requires build ≥ 20348; otherwise fall back to device loopback.
- **Virtual devices (input cables / virtual mic):** need a kernel driver. v1 detects and uses **VB-CABLE** or the open-source **Virtual-Audio-Driver**. **VoiceMeeter's own virtual devices only pass audio while VoiceMeeter's engine runs, so they're useless once we replace it.** A custom driver via `windows-drivers-rs` is v3+ (needs attestation signing / EV cert).

### 6.2 Linux
- Crate: `pipewire` (pipewire-rs, binds libpipewire, an unavoidable C dependency). `cpal` (ALSA) as fallback for non-PipeWire systems (no virtual devices there).
- **Virtual devices without modules:** each virtual input = our own `pw_stream` node with `media.class = Audio/Sink`. Each virtual mic = `Audio/Source/Virtual`. They exist only while `routerd` runs. No `pactl load-module` shelling.
- **Per-app capture:** watch the registry for `Stream/Output/Audio` nodes and create links (`link-factory`) from the app's ports to our strip node. Natively per-app, no loopback module.
- Latency: request `node.latency = 128/48000`. Don't force the global quantum.

### 6.3 Android (the hard one)

**Rust owns everything except what Android forces into Java classes.**

| Piece | Language | Why |
|---|---|---|
| UI | Rust (Makepad) | your choice |
| Mic capture, output playback | Rust → AAudio (`ndk` crate) | low-latency, `InputPreset::VoicePerformance`, `PerformanceMode::LowLatency`, fall back from Exclusive to Shared |
| App-audio capture | **Rust calling Java `AudioRecord` via `jni`** | Only `AudioRecord` accepts `AudioPlaybackCaptureConfiguration`. Rust thread: `attach_current_thread_permanently`, then `read(directByteBuffer, n, READ_BLOCKING)` into Rust memory (zero-copy), then `rtrb` |
| WifiLock, WakeLock, thread priority, MediaProjection calls | Rust via `jni` | plain API calls, no subclassing needed |
| `AudioEngineService extends Service` | **Java (~80 lines)** | Manifest-declared components must be classes in the APK's dex |
| `ProjectionConsentActivity` | **Java (~40 lines)** | `startActivityForResult(createScreenCaptureIntent())`. Lets us avoid depending on whether Makepad's activity forwards `onActivityResult` |

Each Java method is a one-line forward to `native` functions registered by Rust (`RegisterNatives` in `JNI_OnLoad`). No Kotlin, no Gradle logic. **Makepad merged support for an app's own Java + jars + `AndroidManifest.xml.template` into its `work` branch on 2026-10-21 (PR #1313)**, built by `cargo makepad android build`. That's exactly what we need, but it's 2 weeks old, so pin the commit.

**Lifecycle & permissions (Android 14+ rules, safe on 10+):**
1. Manifest: `RECORD_AUDIO`, `FOREGROUND_SERVICE`, `FOREGROUND_SERVICE_MICROPHONE`, `FOREGROUND_SERVICE_MEDIA_PROJECTION`, `POST_NOTIFICATIONS`, `WAKE_LOCK`, `ACCESS_WIFI_STATE`, `CHANGE_WIFI_MULTICAST_STATE`, `INTERNET`. Service `foregroundServiceType="microphone|mediaProjection"`.
2. **Mic:** while the UI is visible, start the FGS with type `microphone`. (A while-in-use FGS can't be started from background.) The mic keeps working with the screen off.
3. **App audio:** consent Activity → result Intent → `startForeground(..., MICROPHONE | MEDIA_PROJECTION)` → *then* `getMediaProjection(resultCode, data)`. On Android 14 the projection type requires consent first and an FGS before `getMediaProjection`. Register `MediaProjection.Callback` and handle `onStop`.
4. The token is **single-use**, so every new capture session needs a fresh consent dialog (Android 14+).
5. **Android 15 QPR1+: projection stops when the screen locks** (per Android docs), so app-audio capture ends while mic capture continues. UI and notification show "App audio paused — tap to resume". **Verify on your phone first; this is the biggest risk to "always running in background".**
6. Battery: request `ignoreBatteryOptimizations` from the UI (expert). Some OEM skins (Xiaomi/Oppo/Vivo) also kill FGS apps. Link dontkillmyapp.com-style instructions in-app.

**Android limits to state up front:** you can't create a virtual mic for other apps without root. Capturing calls/VoIP (`USAGE_VOICE_COMMUNICATION`) isn't possible. Apps that set `allowAudioPlaybackCapture=false` are silent. Capture doesn't mute the original output.

**Per-app capture on Android is possible:** `addMatchingUid(uid)`. The UI lists running apps via `PackageManager`.

### 6.4 UI toolkit decision
- **Desktop: egui (`eframe`, `glow` renderer)**. Mature and well-documented. glow is lighter than wgpu for a 2D mixer.
- **Android: Makepad, as you wanted, with one honest caveat.** "Makepad 2.0" is the runtime-script rewrite (`live_design!` → `script_mod!`, `#[derive(Live)]` → `#[derive(Script)]`). It's still landing on the dev/`work` branches, so expect API churn and pin a commit. The plan keeps the risk small: the Java shim and engine don't depend on the UI toolkit, and the Android UI is a compact view (sources, targets, buffer, meters, record). If Makepad blocks you, eframe on Android is the fallback with only the view code rewritten.
- (Makepad also runs on Windows/Linux. A single-toolkit build is possible later if you end up preferring it.)

---

## 7. Engine (RT mixer)

- Planar `f32`, ≤ 8 channels per strip/bus, block = 128 frames. Device callbacks with other sizes (e.g. WASAPI 480, AAudio burst 96/192) adapt through their port FIFO.
- **Port pool:** at startup allocate `MAX_PORTS` (48) SPSC ring pairs (`rtrb`, 8 ch × 4096 frames f32 = 128 KB each, about 6 MB total). The control plane hands producers/consumers to device threads and sends `Attach{slot}`. Rings are **reused, never freed**, so attach/detach is alloc-free on the RT side.
- Per strip: gate → 3-band RBJ biquad EQ → compressor → gain → pan/channel map → sends (pre/post-fader, solo). Per bus: sum → EQ → gain → meter → output ring + optional recorder ring.
- Parameter smoothing: one-pole on gain/pan (5 ms) to prevent zipper noise.
- `assert_no_alloc` wraps `process_block` in debug/test builds. Load meter = elapsed / block time → `rt_load_permille`.

## 8. Recording
- Any bus with `record = true` feeds a recorder ring. The recorder thread writes **WAV via `hound`** (pure Rust) or FLAC via `flacenc` (pure Rust). Opus-in-Ogg later.
- Android: app-specific external dir (no permission), with optional export to MediaStore via JNI.
- Typical Android use: strip 1 = mic, strip 2 = app audio → bus B1 = LAN to PC, bus B2 = record. Both work at once.

## 9. UI design (both toolkits share `app-core` view-models)

### 9.1 Modes
- **Simple mode:** "Send ▸ [This phone's mic / App audio] ▸ to [discovered PC]" and "Receive ▸ [peer stream] ▸ to [speakers]" cards. One tap to connect. The buffer preset is picked from measured jitter.
- **Expert mode:** the three layouts below, all over the same state. No OS popup windows anywhere; every inspector is inline.

### 9.2 Console Strips view (VoiceMeeter-style, ships first, S7)
- `egui::ScrollArea::horizontal()`: strips on the left, buses on the right, scrolls smoothly on narrow screens.
- Strip column is 125 px wide: label, source picker, vertical stereo/N-ch meter, fader, Mute/Solo, routing buttons `A1..An / B1..Bn` (one per bus).
- **Collapse (`«`)** shrinks a strip/bus to a **28 px meter pillar**: vertical meter, Mute, and the channel name drawn vertically.
- **Right-click a routing button** → inline send-gain knob + pre/post toggle on the strip.
- **Right-click the header/FX slot** → inline side drawer attached to that strip: gate / 3-band EQ / compressor. For LAN strips it also shows the stream inspector (§9.5).

### 9.3 Routing Matrix view (S7)
- Grid with strips as rows and buses as columns. A cell click toggles the send; dragging on a cell sets send gain. Fastest way to audit routing.

### 9.4 Patchbay Cards view (polish phase, S9)
- Two vertical `ScrollArea`s: Sources (left), Destinations (right).
- Collapsed card (~52 px): device/app icon, editable label, live level bar as card background, fader, Mute/Solo, connection socket.
- Drag from source socket to bus socket → cubic-bezier wire (`egui::Painter`). Right-click a wire → send gain.
- `▾` or right-click on a card → **inline accordion drawer**: per-bus send faders, DSP, and for LAN streams the stream inspector.
- RichMotion: cards tilt slightly with `pointer.velocity()`, reorder with spring interpolation, particles move along wires while audio flows. EcoReducedMotion: instant snap, static anti-aliased wires.

### 9.5 LAN stream inspector
Codec (`PCM`/`Opus`), data type, channels, frames/pkt, DSCP, **Network Buffer slider** (Optimal 6 / Fast 12 / Medium 24 / Slow 48 / VerySlow 96 / Custom ms), packet duplication toggle. Live telemetry: buffer ms vs target, drift ppm, lost/late/underruns, last-packet age, RSSI/link speed.

### 9.6 Top toolbar
`[ Simple | Expert ]`, layout toggle `[ Strips | Matrix | Patchbay ]`, graphics profile `[ Rich | Eco | Custom ]`, discovered-peers quick-connect menu, engine status (RT load ‰, overruns), record button.

### 9.7 Touch & small screens (Android, and narrow desktop windows)
When viewport width < 600 logical px or touch input is detected: fader grab handles ≥ 44 px, kinetic touch scrolling, strips default to collapsed pillars, and inspectors open full-width instead of side drawers.

### 9.8 UI-side contract (lives in `app-core`, NOT `audio-types`)
```rust
// SECTION: UI settings (app-core/src/ui_state.rs)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum UiMode { #[default] Simple, Expert }

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LayoutMode { #[default] ConsoleStrips, Matrix, PatchbayCards }

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum GraphicsProfile { #[default] RichMotion, EcoReducedMotion, Custom }

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct GraphicsSettings {
    pub profile: GraphicsProfile,
    pub active_fps: u8,                 // * Rich 60 / Eco 30
    pub silent_fps: u8,                 // * Rich 15 / Eco 5
    pub card_spring_physics: bool,
    pub wire_particles: bool,
    pub smooth_transitions: bool,
    pub meter_peak_hold: bool,
}
impl GraphicsSettings {
    pub const RICH: Self = Self { profile: GraphicsProfile::RichMotion, active_fps: 60, silent_fps: 15,
        card_spring_physics: true, wire_particles: true, smooth_transitions: true, meter_peak_hold: true };
    pub const ECO: Self = Self { profile: GraphicsProfile::EcoReducedMotion, active_fps: 30, silent_fps: 5,
        card_spring_physics: false, wire_particles: false, smooth_transitions: false, meter_peak_hold: false };
}

/// ? Per-channel view state, kept out of StripConfig so the engine config stays UI-agnostic.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelViewState { pub collapsed_in_strips: bool, pub expanded_in_cards: bool, pub card_order: u8 }

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UiLayout {
    pub mode: UiMode,
    pub layout: LayoutMode,
    pub graphics: Option<GraphicsSettings>, // None => RICH on desktop, ECO on Android
    pub strips: [ChannelViewState; MAX_STRIPS],
    pub buses: [ChannelViewState; MAX_BUSES],
}
// !SECTION: UI settings
```

### 9.9 Repaint governor
- UI process closed (daemon keeps running) → nothing runs.
- Minimized/hidden, where the platform reports it → **0 repaints** (no `request_repaint*` call).
- Wayland, where minimized is `None`: unfocused → `silent_fps`.
- Visible + `any_audio_active` → `request_repaint_after(1 / active_fps)`.
- Visible + silent > 500 ms → `silent_fps`.
- Never an unconditional `request_repaint()`. On Android, Makepad stops drawing when the Activity is backgrounded; the engine is unaffected because it lives in the FGS.

---

## 10. Workspace layout

```text
lan-audio-router/
├── CLAUDE.md  PLAN.md  Cargo.toml
├── crates/
│   ├── audio-types/        # contract (implemented + tested, see file)
│   ├── audio-engine/       # RT mixer, DSP, port pool, drift resampler
│   ├── net-vban/           # packet codec, RX/TX threads, reorder+jitter, Opus (feature), VBAN-TEXT/SERVICE, mdns
│   ├── platform-io/        # trait + mock | wasapi | pipewire | cpal-fallback | aaudio + jni playback capture
│   ├── app-core/           # ControlApi, config persistence (TOML), view-models, IPC protocol (postcard)
│   ├── routerd/            # desktop engine daemon + tray helper
│   ├── router-ui/          # egui desktop client
│   ├── router-cli/         # probe, list devices, headless config
│   └── android-app/        # Makepad UI + JNI glue (cdylib)
│       └── resources/android/
│           ├── AndroidManifest.xml.template
│           └── java/dev/f/lanrouter/{AudioEngineService,ProjectionConsentActivity}.java
```

Key crates: `rtrb`, `rubato` 1.x, `assert_no_alloc`, `socket2`, `mdns-sd`, `atomic-wait`, `opus` (feature), `wasapi`, `windows`, `pipewire`, `cpal`, `ndk` (audio feature), `jni`, `robius-android-env` (JavaVM/Activity handle under Makepad), `makepad-widgets` (git, pinned), `eframe`/`egui`, `tray-icon`, `interprocess`, `postcard`, `serde`, `toml`, `hound`, `flacenc`, `snow` (later). Pin versions with `cargo add` at the start of each session.

---

## 11. Session roadmap

Order: audio + network core first (S1–S4), proven against VoiceMeeter on Linux (S5) before spending time on UI or Android. Each session below is a self-contained prompt. Start every one with *"Read CLAUDE.md and PLAN.md first."* Finish every one by running the CLAUDE.md verification commands and reporting which PLAN §12 items still need hardware.

### S1 — Workspace, contract, RT mixer (`audio-engine`)
- Root `Cargo.toml` (edition 2024, shared `[workspace.dependencies]`), with members per §10. Crates that aren't in scope yet get stub `lib.rs` files.
- `audio-types`: drop in the provided `lib.rs` + `Cargo.toml` unchanged.
- `audio-engine`:
  - `PortPool` (48 pre-allocated `rtrb` pairs, 8 ch × 4096 frames).
  - `MixerEngine { strips, buses, cmd_rx: rtrb::Consumer<EngineCommand>, telemetry: Arc<Telemetry> }`.
  - `process_block(frames)`: drain commands → read attached input ports → per-strip gate / RBJ 3-band EQ / compressor / smoothed gain / constant-power pan or channel map → meters → sends (pre/post, solo) → bus EQ + gain → bus meters → output ports + recorder ports. Set `any_audio_active` when any channel > −90 dBFS. Update `rt_load_permille`.
- Tests: 10 000 × `process_block(128)` inside `assert_no_alloc` while another thread floods commands. DSP golden tests: biquad response at 3 frequencies within 0.1 dB, gate opens/closes with hysteresis, compressor static curve.
- Bench: 16 × 16 stereo, all DSP on, < 50 µs per block in release.
- **Accept:** `cargo test -p audio-types -p audio-engine -- --nocapture`

### S2 — Clock drift (`audio-engine::drift`)
- `DriftResampler` trait, with two implementations: cubic Hermite (ours) and rubato `Async::new_poly(.., PolynomialDegree::Cubic, .., FixedAsync::Output)` via `process_into_buffer`.
- `DriftController`: EMA τ ≈ 1 s on FIFO fill, PI → ratio, clamp ±1000 ppm, slew ≤ 10 ppm/block. Publish `drift_ppm`.
- `ClockDomainAdapter` = FIFO + resampler + controller. Used by every non-master port.
- Sim tests: producer at 48 005 Hz, consumer at 47 995 Hz, 10 simulated minutes → fill within ±1 ms of target after a 5 s settle, 0 underruns. Same with ±100 ppm swapped. A THD+N check of a 1 kHz sine through the resampler stays < −80 dB.
- Benchmark both resamplers and keep the cheaper one that passes.
- **Accept:** `cargo test -p audio-engine drift -- --nocapture`

### S3 — Network (`net-vban`)
- `packet.rs`: PCM ↔ f32 for every data type in §4.1 (TX: Int16/Int24/Float32). Auto frames/pkt via `vban_max_frames`. Opus framing with the `OPUS_USER_TAG` prefix (feature `opus`).
- `rx.rs`: one socket per port, blocking recv with a 5 ms timeout, demux `(src_ip, stream_name)` → per-stream reorder window (4 slots) → play-out FIFO. Counts lost/late/duplicate. Enforces the IP allow policy.
- `jitter.rs`: target from `effective_target_ms`. Underrun → 3 ms fade + rebuffer. Overrun → crossfade-drop. Optional adaptive target.
- `tx.rs`: futex or 1 ms tick wake, packetize, `send_to`, optional duplication. DSCP: `IP_TOS` on unix, qWAVE behind `cfg(windows)`.
- `service.rs`: VBAN-SERVICE ping/reply, a VBAN-TEXT parser for the `Strip[i].Gain/Mute`, `Bus[i].Gain/Mute` subset, and the 2 s beacon.
- `discovery.rs`: `mdns-sd` `_lanrouter._udp.local.` with TXT keys per §4.5.
- Tests: loopback UDP at `Fast` with 0–12 ms jitter, 1 % loss, 0.5 % reorder → 0 underruns over 5 000 packets. Byte-exact header fixtures for VoiceMeeter-style packets: Int16 stereo 256 frames, Float32 8 ch 44 frames.
- **Accept:** `cargo test -p net-vban` (and `--features opus` if libopus builds)

### S4 — Control plane (`platform-io` mock, `app-core`, `routerd`, `router-cli`)
- `platform-io`: `AudioBackend` trait (enumerate devices/apps, open input/output into a port, create virtual endpoints, capabilities). `MockBackend` uses sine/noise sources, a configurable clock offset, and injectable dropouts.
- `app-core`:
  - `ControlApi` (commands, `StateSnapshot`, 30 Hz `MeterSnapshot`) and the `UiLayout`/`GraphicsSettings` from §9.8.
  - TOML profile save/load in the OS config dir (round-trip test).
  - IPC protocol (postcard over `interprocess` local sockets).
- `routerd`: owns the engine + net + backend, serves IPC, and has a single-instance lock.
- `router-cli`: `devices`, `apps`, `status`, `route`, and `probe <peer>` (VBAN-SERVICE RTT histogram + click-loop).
- Tests: two `routerd` instances on loopback (mock backend) stream to each other. The probe reports latency within ±1 block of the configured pipeline.
- **Accept:** `cargo test -p platform-io -p app-core -p routerd -p router-cli`

### S5 — Linux PipeWire backend
- Device enumeration + streams with `node.latency = 128/48000`.
- Virtual input = own `Audio/Sink` node. Virtual mic = own `Audio/Source/Virtual` node.
- Per-app capture by linking `Stream/Output/Audio` ports to our strip node.
- RT priority via rtkit. `cpal` ALSA fallback when PipeWire is absent.
- Manual checklist: VoiceMeeter on Windows ⇄ Linux in both directions at Int16 128/256 frames, Float32. Measure with `router-cli probe`.
- **Accept:** `cargo test -p platform-io`, plus the manual checklist results recorded in `docs/interop.md`

### S6 — Windows WASAPI backend
- `wasapi` + `windows`: event-driven shared mode, `IAudioClient3` minimum period, exclusive as an option, MMCSS "Pro Audio".
- Process loopback include/exclude (parent PID resolution), gated on build ≥ 20348, with device-loopback fallback.
- qWAVE QoS. Detect VB-CABLE / Virtual-Audio-Driver endpoints and expose them as virtual input/mic.
- Tray helper (`tray-icon`) that spawns or closes `router-ui`.
- **Accept:** `cargo check --workspace --target x86_64-pc-windows-gnu`, plus manual on Windows

### S7 — Desktop UI (`router-ui`, egui/eframe glow)
- §9.1 Simple mode, §9.2 Console Strips, §9.3 Matrix, §9.5 inspector, §9.6 toolbar, §9.7 narrow layout, §9.9 governor. Connects to `routerd` over IPC and shows a "daemon not running → start" banner.
- Headless tests (`egui_kittest`):
  - Fader drag emits `SetStripGain`.
  - Collapse makes the column 28 px.
  - Right-click on a routing button opens the send knob.
  - The governor returns "no repaint" when minimized in both Rich and Eco, and `silent_fps` after 500 ms of silence.
- **Accept:** `cargo test -p router-ui`

### S8 — Android (`android-app`, Makepad pinned commit)
- Java shim per §6.3 + `AndroidManifest.xml.template`. `JNI_OnLoad` + `RegisterNatives`.
- AAudio mic (`VoicePerformance`, LowLatency, Exclusive→Shared fallback) and output.
- Playback capture: Rust thread driving `AudioRecord` over JNI with a direct ByteBuffer. `AllMediaAndGames` / per-UID filter, app list via `PackageManager`.
- FGS lifecycle steps 1–6. WifiLock low-latency + partial WakeLock while streaming. Recorder to the app-specific dir.
- Compact Makepad UI: Simple mode, sources, targets, buffer, meters, record, "app audio paused — tap to resume".
- **Accept:** `cargo makepad android build` succeeds, plus the §12 device checklist

### S9 — Polish
§9.4 Patchbay Cards with motion profiles, adaptive jitter target, VBAN-TEXT remote control of VoiceMeeter peers, `ControlApi` over LAN (Noise/`snow` pairing), FLAC/Opus recording.

---

## 12. Must-verify-on-device checklist (things no cloud test can prove)
1. Android playback-capture latency on your phone (click-loop probe). If > 40 ms, set expectations for the app-audio path.
2. Does locking the screen stop MediaProjection audio capture on your Android version?
3. Does your OEM kill the FGS after N minutes with the screen off?
4. `WIFI_MODE_FULL_LOW_LATENCY`: ping RTT histogram with vs without (expect p99 to drop from 100+ ms to < 15 ms if power-save was the cause).
5. VoiceMeeter interop: does VoiceMeeter receive our Int16/Int24/Float32 at 128 and 256 frames? Does it reply to our VBAN-SERVICE ping? Do VBAN-TEXT commands work?
6. Windows: which build are you on (process loopback needs ≥ 20348)? Is VB-CABLE installed?

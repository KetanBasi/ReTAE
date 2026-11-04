# Android test session (S8)

The Android code was written and checked on Linux: `cargo clippy --target aarch64-linux-android`,
host tests of the pure rules (`platform-io::android_rules`, `android-app::{lifecycle, natives,
view_model}`), `javac` of the three Java files against the API 35 `android.jar`, and `javap` of every
JNI class, method and signature the Rust code calls. **None of it has run on a phone**, and two
things stand between the code and a phone (see `NOTES.md` J5 and J6): the Java is not yet packed
into the APK, and the Makepad screen is not written. Do this list after those two are done, ideally
in one session with `docs/windows-testing.md` and the VoiceMeeter table in `docs/interop.md`.

Write results into the tables, and move anything that fails into `TODO.md`.

| Item | Value |
|---|---|
| Phone model, Android version, security patch | |
| Vendor skin (Xiaomi / Oppo / Vivo / Samsung / Pixel) | |
| Wi-Fi: access point, band, channel width | |
| Build installed from (`cargo makepad android build` + the Java step) | |

## 1. Build and install

```bash
cargo makepad android build --release -p android-app
adb install -r <the apk the build prints>
adb logcat -s makepad:I retae:I AndroidRuntime:E
```

`glue::log` prints to stderr, which Makepad sends to logcat under the tag `makepad`; lines start with
`retae:`. The Java classes `dev.f.lanrouter.*` must be in the APK (`unzip -l`, then `dexdump` or
`apkanalyzer dex packages`). If starting the router crashes with `ClassNotFoundException` or
`UnsatisfiedLinkError`, that is J5/J8, not the audio code.

## 2. Lifecycle (PLAN §12.2 and §12.3)

| # | Step | Expect | Result |
|---|---|---|---|
| 1 | First launch, tap **Start** | microphone permission asked, then a notification "Router running"; no crash | |
| 2 | Press Home, lock the screen for 10 minutes while sending the microphone to a PC | the PC still hears it | |
| 3 | Turn on **app audio**, accept the screen-capture dialog | notification stays; the PC hears a YouTube / music app | |
| 4 | Lock the screen with app audio on (Android 15 QPR1 or newer) | app audio stops, microphone continues, banner and notification read "App audio paused — tap to resume" | |
| 5 | Tap the notification | the consent dialog opens; accepting resumes app audio | |
| 6 | Same as 4 on Android 10–14 | write down whether the projection survives the lock | |
| 7 | Screen off for 30 minutes on battery | is the service still running (`adb shell dumpsys activity services dev.f.lanrouter`)? OEM kill? | |
| 8 | Deny the microphone permission | a clear message, no service | |
| 9 | Deny the screen-capture dialog | "Screen-capture permission was refused", microphone unaffected | |

## 3. Audio (PLAN §12.1)

| # | Check | Result |
|---|---|---|
| 1 | The stream `detail()` for the microphone: exclusive or shared, which preset, burst and buffer frames | |
| 2 | A mono microphone into the stereo Send card: both sides carry sound, level looks right | |
| 3 | Speaker / wired headset / USB / Bluetooth listed and playing (Receive card) | |
| 4 | Click-loop probe on the app-audio path (play a click in an app, find it on the PC): milliseconds | |
| 5 | The same for the microphone and for a PC-to-phone stream | |
| 6 | An app with capture disabled (`allowAudioPlaybackCapture=false`, e.g. some streaming apps) is silent, not an error | |
| 7 | Per-app filter (`OnlyUid`) captures that app and not another one | |
| 8 | A phone call or a VoIP app is **not** captured (documented limit) | |
| 9 | Underruns / overruns after 10 minutes (`LanRxLevels`, engine counters) | |

If app-audio latency is above 40 ms, say so in `NOTES.md` as the expectation for that path (PLAN §12.1).

## 4. Network (PLAN §12.4 and §12.5)

| # | Check | Result |
|---|---|---|
| 1 | `ping` the phone from the PC for a minute with the screen off, with and without the low-latency Wi-Fi lock (comment the lock out, or use `adb shell dumpsys wifi`): p50 / p99 | |
| 2 | Fast (12 ms) and Medium (24 ms) presets on this access point: underruns per minute | |
| 3 | VoiceMeeter on the PC receives the phone's microphone (Int16 stereo, 128 frames) | |
| 4 | The phone plays VoiceMeeter's VBAN stream | |
| 5 | `cargo test -p net-vban` run on the device (needs `cargo-dinghy` or a termux build) | |

## 5. Thread priority

`Process.setThreadPriority(THREAD_PRIORITY_URGENT_AUDIO)` is requested for the VBAN receive and
transmit threads. Check with `adb shell ps -T -p <pid> -o TID,NI,NAME` that those threads
(named `vban-rx-<port>` and `vban-tx-<stream>`) show a nice value of -19 (the call may be refused
on some builds; the code ignores a refusal).

# NOTES — decisions, known limits, fixable issues

Living document, not a changelog. Edit an entry in place when its status changes and delete it when
fixed (git history is the log). Tags: **[fixable]** we know how · **[by design]** intentional, revisit
only if a goal changes · **[unproven]** needs hardware or another machine, see `TODO.md`.

## Decisions
- **A1** Commits carry no AI attribution. S1's commit `5cc5900` still has trailers; the owner chose to leave it, do not ask again.
- **A2** The engine is 48 kHz only. Peers at other rates are dropped and counted (`wrong_rate`); backends must open devices at 48 kHz.
- **A3** RX trust: a stream naming a source IP trusts exactly that IP; wildcard streams accept only `RxPolicy.allowed` (or anyone with the expert `accept_any`). VBAN-TEXT follows the same rule. VBAN-SERVICE is open (discovery needs it) but replies are LAN-only and rate-limited.
- **A4** TX wakes on a 1 ms `park_timeout` tick, not `atomic-wait` (no timed wait, and the RT thread may not make a syscall).
- **A5** The drift resampler is our own cubic Hermite; rubato is a dev-dependency only (benchmarks and comparison).
- **A6** Contract additions to `audio-types` so far: `ClockDomain` + `EngineCommand::SetStripClock` (S2), `LanTxConfig.duplicate` (S3, `serde(default)`).

## audio-engine (S1)
- **E1** [by design] A port ring is 32 768 f32: 8 ch × 4096 frames = 85 ms at 8 channels (341 ms at stereo). An 8-channel strip with a target above 85 ms (VerySlow = 96 ms, large Custom) cannot be buffered. Fix if needed: bigger rings (48 × 128 KB = 6 MB today) or clamp the target by channel count.
- **E2** [unproven] Worst-case 16×16 bench is 27–37 µs per block (41.6 µs with a drift adapter on every strip; budget 50 µs), measured on a shared 2.1 GHz VM in release.
- **E3** [by design] `assert_no_alloc` guards debug and test builds only.
- **E4** [fixable] Rejected (invalid) commands are counted in `EngineStats` but nothing shows them yet; surface them in the inspector (S7).

## drift (S2)
- **D1** [fixable] With chunky producers (one packet ≈ one block, e.g. the 128-frame default) the fill seen at block starts is a staircase the 1 s EMA cannot smooth: the smoothed fill swings up to half a packet (2.1 ms at 208 ppm). At Optimal (6 ms) that leaves ~0.3 ms of headroom. Fix: give the controller a fill estimate that does not depend on packet phase (arrival timestamps from the RX thread, or fill averaged over whole packet periods). Not done in S3.
- **D2** [by design] Hermite is for ±1000 ppm drift only (THD+N −91.7 dB at 1 kHz, −49 dB at 5 kHz, −29 dB at 10 kHz). It is not a sample-rate converter; if one is ever needed use rubato behind `DriftResampler`.
- **D3** [by design] Offsets beyond ±1000 ppm saturate the controller; the adapter then relies on its underrun/overrun handling.
- **D4** The controller uses conditional-integration anti-windup (amended in S3 after the jitter sims showed windup). Undershoot after a high start stays < 1.6 ms in the regression test.

## net-vban (S3)
- **N1** [fixable] Optimal (6 ms ≈ 2 packets): one lost packet is concealed, two in a row underrun, because nothing arrives behind the hole to reveal it. Fix: predictive concealment of an overdue expected packet when the ring is about to run dry (cap ~4 in a row, a late arrival counts as late), then tighten the `jitter_sim` thresholds. Fast and above are unaffected.
- **N2** [by design] Fast (12 ms) with jitter equal to the buffer is not guaranteed underrun-free (sim: 54/60 independent-delay and 48/60 in-order runs clean, failures mostly start-up). Exactly 0 underruns when the buffer can absorb the jitter (7 ms independent, 4 ms in-order). A start-up pre-roll was tried and rejected.
- **N3** [fixable] `AdaptiveTarget` (jitter.rs) is written and tested but not wired to the RX thread or telemetry (planned for S9).
- **N4** [fixable] RX/TX threads run at default priority. PLAN §4.3 wants MMCSS "Pro Audio" (Windows), rtkit (Linux), `THREAD_PRIORITY_URGENT_AUDIO` (Android): done with S5/S6/S8. It matters: in the dev container (a microVM) the 1 ms TX tick sometimes wakes 3–30 ms late, which is more than the 12 ms Fast buffer holds (one underrun, then a backlog burst that overruns).
- **N5** [fixable] Sockets are not rebound after an interface change (Wi-Fi roaming, NIC unplug); errors are counted (`socket_errors`) and the thread backs off 5 ms. Fix: rebind after N consecutive errors.
- **N6** [fixable] The beacon goes to `255.255.255.255:6980` only, which leaves through the default interface. Multi-homed hosts need per-interface directed broadcasts (`Service::set_targets`); `app-core` should enumerate interfaces.
- **N7** [by design] VBAN-TEXT handles only set-commands for `Strip[i]`/`Bus[i]` `Gain` (clamped −60…+12 dB) and `Mute`; queries and every other VoiceMeeter parameter are ignored and counted. Wider coverage is S9.
- **N8** [by design] Opus: mono/stereo, `RESTRICTED_LOWDELAY` (PLC only, no FEC); the header frame count is informational. Audio is IPv4 unicast only.
- **N9** [unproven] The PING0 layout, VBAN-TEXT header and service type/function bytes come from the public protocol description, not a capture. Windows qWAVE and the 1 ms timer are compile-checked only.
- **N10** Live mDNS test is `#[ignore]` (needs multicast); run `cargo test -p net-vban --lib -- --ignored mdns`.
- **N11** The mixer loopback test (`tests/loopback.rs`) requires 0 engine underruns/overruns only when the host was quiet (worst wake-up delay < 6 ms, measured in the test); on a noisy host it prints `NOTE: noisy host`, allows up to 20, and still requires 0 lost/late/concealed packets. On a real host it should print no NOTE; if it does, that is a finding (see N4).
- **N12** Sockets ask for 256 KB kernel buffers (PLAN §4.3), best effort: Linux caps the request at `net.core.rmem_max`/`wmem_max`. `SO_REUSEADDR` is deliberately not set (on Linux it would let two sockets share a UDP port and split unicast between them).

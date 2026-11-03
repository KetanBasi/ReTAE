//! Text output of every subcommand. Pure functions from data to `String`, so tests can read them
//! without a terminal.

use app_core::types::{
    ClickLoopReport, LinkState, MeterSnapshot, ProbeReport, ProbeRequest, RttReport, StateSnapshot,
};
use audio_types::{SinkEndpoint, SourceEndpoint};
use platform_io::{AppInfo, DeviceInfo, DeviceKind};
use std::fmt::Write as _;

// ? `write!` into a `String` cannot fail, so the results are dropped with `let _ =`.

pub fn devices(list: &[DeviceInfo]) -> String {
    if list.is_empty() {
        return "no audio devices\n".into();
    }
    let mut s = format!("{:<18} {:<7} {:>3}  {}\n", "ID", "KIND", "CH", "NAME");
    for d in list {
        let kind = match d.kind {
            DeviceKind::Input => "input",
            DeviceKind::Output => "output",
        };
        let _ = writeln!(
            s,
            "{:<18} {:<7} {:>3}  {}{}",
            d.id,
            kind,
            d.channels,
            d.name,
            if d.is_default { "  (default)" } else { "" }
        );
    }
    s
}

pub fn apps(list: &[AppInfo]) -> String {
    if list.is_empty() {
        return "no capturable applications\n".into();
    }
    let mut s = format!("{:>7}  {:<8} {}\n", "PID", "STATE", "NAME");
    for a in list {
        let _ = writeln!(
            s,
            "{:>7}  {:<8} {}",
            a.pid,
            if a.playing { "playing" } else { "idle" },
            a.name
        );
    }
    s
}

fn source_text(source: &SourceEndpoint) -> String {
    match source {
        SourceEndpoint::Unassigned => "-".into(),
        SourceEndpoint::HardwareInput { device_id } => format!("input {}", device_id.as_str()),
        SourceEndpoint::DeviceLoopback { device_id } => format!("loopback {}", device_id.as_str()),
        SourceEndpoint::ProcessLoopback { pid, exe_name, .. } => {
            format!("app {} (pid {pid})", exe_name.as_str())
        }
        SourceEndpoint::VirtualInput { index } => format!("virtual input {index}"),
        SourceEndpoint::AndroidMic { .. } => "android mic".into(),
        SourceEndpoint::AndroidPlaybackCapture { .. } => "android playback capture".into(),
        SourceEndpoint::LanIn(rx) => {
            let ip = rx.source_ipv4;
            let from = if ip == [0; 4] {
                "any".to_owned()
            } else {
                format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
            };
            format!("lan {:?} from {from}", rx.stream_name.as_str())
        }
    }
}

fn sink_text(sink: &SinkEndpoint) -> String {
    match sink {
        SinkEndpoint::Unassigned => "-".into(),
        SinkEndpoint::HardwareOutput { device_id } => format!("output {}", device_id.as_str()),
        SinkEndpoint::VirtualMic { index } => format!("virtual mic {index}"),
        SinkEndpoint::LanOut(tx) => {
            let ip = tx.peer_ipv4;
            format!(
                "lan {:?} to {}.{}.{}.{}:{}",
                tx.stream_name.as_str(),
                ip[0],
                ip[1],
                ip[2],
                ip[3],
                tx.port
            )
        }
    }
}

fn link_text(link: &LinkState) -> String {
    match link {
        LinkState::Idle => "idle".into(),
        LinkState::Running => "running".into(),
        LinkState::Failed(why) => format!("FAILED: {why}"),
    }
}

/// A level in dB, with silence shown as `-inf` instead of a huge negative number.
fn db(v: f32) -> String {
    if v.is_finite() && v > -120.0 {
        format!("{v:+.1}")
    } else {
        "-inf".into()
    }
}

fn ip_text(ip: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3])
}

pub fn status(state: &StateSnapshot, meters: &MeterSnapshot) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "{} ({} backend), VBAN receive port {}",
        state.name, state.backend, state.rx_port
    );
    let e = &meters.engine;
    let _ = writeln!(
        s,
        "engine: load {:.1}%  overruns {}  input under/over {}/{}  output over {}  rejected {}",
        e.rt_load_permille as f32 / 10.0,
        e.rt_overruns,
        e.input_underruns,
        e.input_overruns,
        e.output_overruns,
        e.rejected_commands
    );

    let _ = writeln!(s, "\ninputs");
    let mut any = false;
    for (i, strip) in state.strips.iter().enumerate() {
        if matches!(strip.config.source, SourceEndpoint::Unassigned) {
            continue;
        }
        any = true;
        let level = meters
            .strips
            .get(i)
            .map_or("-inf".into(), |m| db(m.peak_db[0]));
        let _ = writeln!(
            s,
            "  {i:>2}  {:<12} {:<34} {:<8} {:>7} dB  gain {:+.1}{}{}",
            strip.config.label.as_str(),
            source_text(&strip.config.source),
            link_text(&strip.link),
            level,
            strip.config.gain_db,
            if strip.config.mute { "  MUTE" } else { "" },
            if strip.config.solo { "  SOLO" } else { "" },
        );
        if let Some(lan) = meters
            .lan_rx
            .get(i)
            .filter(|l| l.last_packet_ms_ago != u32::MAX)
        {
            let _ = writeln!(
                s,
                "      jitter buffer {:.1}/{:.1} ms  drift {:+.0} ppm  lost {}  late {}  underruns {}",
                lan.buffer_ms,
                lan.target_ms,
                lan.drift_ppm,
                lan.packets_lost,
                lan.packets_late,
                lan.underruns
            );
        }
    }
    if !any {
        let _ = writeln!(s, "  (none assigned)");
    }

    let _ = writeln!(s, "\noutputs");
    let mut any = false;
    for (i, bus) in state.buses.iter().enumerate() {
        if matches!(bus.config.sink, SinkEndpoint::Unassigned) {
            continue;
        }
        any = true;
        let level = meters
            .buses
            .get(i)
            .map_or("-inf".into(), |m| db(m.peak_db[0]));
        let _ = writeln!(
            s,
            "  {i:>2}  {:<12} {:<34} {:<8} {:>7} dB  gain {:+.1}{}{}",
            bus.config.label.as_str(),
            sink_text(&bus.config.sink),
            link_text(&bus.link),
            level,
            bus.config.gain_db,
            if bus.config.mute { "  MUTE" } else { "" },
            if bus.config.record { "  REC" } else { "" },
        );
        if let Some(tx) = bus.tx {
            let _ = writeln!(
                s,
                "      sent {} packets / {} bytes  errors {}  dropped frames {}",
                tx.packets, tx.bytes, tx.send_errors, tx.dropped_frames
            );
        }
    }
    if !any {
        let _ = writeln!(s, "  (none assigned)");
    }

    let allowed = if state.policy.accept_any {
        "anyone".to_owned()
    } else if state.policy.effective.is_empty() {
        "nobody".to_owned()
    } else {
        state
            .policy
            .effective
            .iter()
            .map(|ip| ip_text(*ip))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let _ = writeln!(s, "\nmay send audio here: {allowed}");

    if !state.peers.is_empty() {
        let _ = writeln!(s, "\npeers (answered a VBAN-SERVICE ping)");
        for p in &state.peers {
            let rtt = p.rtt_ms.map_or("-".into(), |r| format!("{r:.2} ms"));
            let _ = writeln!(
                s,
                "  {:<22} {:<14} {:<18} rtt {rtt}{}",
                p.addr.to_string(),
                p.device_name,
                p.application,
                if p.is_lan_router { "  (ReTAE)" } else { "" }
            );
        }
    }
    if !state.discovered.is_empty() {
        let _ = writeln!(s, "\ndiscovered on the LAN");
        for d in &state.discovered {
            let addr = d.vban_addr.map_or("?".into(), |a| a.to_string());
            let _ = writeln!(s, "  {:<20} {:<10} v{:<8} {addr}", d.name, d.os, d.version);
        }
    }
    if !state.seen_senders.is_empty() {
        let _ = writeln!(s, "\nincoming, not assigned to any input");
        for q in &state.seen_senders {
            let _ = writeln!(
                s,
                "  {:<16} {:<16} {} ch @ {} Hz  {} packets",
                ip_text(q.ip),
                q.stream_name,
                q.channels,
                q.sample_rate,
                q.packets
            );
        }
    }
    s
}

pub fn routes(state: &StateSnapshot) -> String {
    let mut s = String::new();
    let mut count = 0;
    for (si, strip) in state.strips.iter().enumerate() {
        for (bi, send) in strip.config.sends.iter().enumerate() {
            if !send.enabled {
                continue;
            }
            count += 1;
            let bus = state.buses.get(bi);
            let _ = writeln!(
                s,
                "strip {si} {:<12} -> bus {bi} {:<12} {:+.1} dB  {}-fader",
                strip.config.label.as_str(),
                bus.map_or("", |b| b.config.label.as_str()),
                send.gain_db,
                if send.pre_fader { "pre" } else { "post" }
            );
        }
    }
    if count == 0 {
        s.push_str("no routes are on\n");
    }
    s
}

// SECTION: Probe
/// What to print before a probe that takes seconds, so the terminal does not look hung.
pub fn probe_wait(req: &ProbeRequest) -> String {
    let mut parts = Vec::new();
    if let Some(peer) = req.peer {
        // ? Peers answer one ping per 250 ms, and the probe leaves 300 ms between pings.
        let secs = f32::from(req.pings) * 0.3;
        parts.push(format!(
            "pinging {peer} {} times (about {secs:.0} s)",
            req.pings
        ));
    }
    if req.click_loop {
        parts.push(format!("sending {} clicks through the mixer", req.clicks));
    }
    parts.join("; ")
}

pub fn probe(report: &ProbeReport) -> String {
    let mut s = String::new();
    if let Some(r) = &report.rtt {
        s.push_str(&rtt(r));
    }
    if let Some(c) = &report.click_loop {
        if !s.is_empty() {
            s.push('\n');
        }
        s.push_str(&click_loop(c));
    }
    for n in &report.notes {
        let _ = writeln!(s, "note: {n}");
    }
    if s.is_empty() {
        s.push_str("nothing was measured\n");
    }
    s
}

fn rtt(r: &RttReport) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "VBAN-SERVICE round trip: {}/{} answered",
        r.received, r.sent
    );
    if r.received == 0 {
        s.push_str("  no answer: wrong address or port, a firewall, or the peer is not running\n");
        return s;
    }
    let _ = writeln!(
        s,
        "  min {:.2}  p50 {:.2}  p99 {:.2}  max {:.2}  jitter {:.2} ms",
        r.min_ms, r.p50_ms, r.p99_ms, r.max_ms, r.jitter_ms
    );
    s.push_str(&histogram(&r.samples_ms));
    s
}

/// Up to 8 bars between the fastest and slowest sample, `#` scaled to 40 columns.
fn histogram(samples: &[f32]) -> String {
    const BARS: usize = 8;
    const WIDTH: usize = 40;
    let finite: Vec<f32> = samples.iter().copied().filter(|v| v.is_finite()).collect();
    let (Some(lo), Some(hi)) = (
        finite.iter().copied().reduce(f32::min),
        finite.iter().copied().reduce(f32::max),
    ) else {
        return String::new();
    };
    // ? When every sample is the same the range is zero; one bar is all that is needed.
    let bars = if hi - lo < 1e-3 {
        1
    } else {
        BARS.min(finite.len())
    };
    let step = ((hi - lo) / bars as f32).max(1e-3);
    let mut counts = vec![0usize; bars];
    for v in &finite {
        let i = (((v - lo) / step) as usize).min(bars - 1);
        counts[i] += 1;
    }
    let most = counts.iter().copied().max().unwrap_or(1).max(1);
    let mut s = String::new();
    for (i, n) in counts.iter().enumerate() {
        let from = lo + step * i as f32;
        let len = (n * WIDTH).div_ceil(most).min(WIDTH);
        let _ = writeln!(
            s,
            "  {from:>7.2} - {:>7.2} ms | {:<w$} {n}",
            from + step,
            "#".repeat(if *n == 0 { 0 } else { len }),
            w = WIDTH
        );
    }
    s
}

fn click_loop(c: &ClickLoopReport) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "click loop through this machine's mixer: {}/{} clicks found",
        c.clicks_found, c.clicks_sent
    );
    if c.clicks_found == 0 {
        s.push_str("  no click came out: the probe path did not carry audio\n");
    } else {
        let _ = writeln!(
            s,
            "  min {:.2}  p50 {:.2}  max {:.2} ms   configured {:.2} ms   (one block = {:.2} ms)",
            c.min_ms, c.p50_ms, c.max_ms, c.expected_ms, c.block_ms
        );
        let _ = writeln!(
            s,
            "  {}",
            if c.within_one_block {
                "OK: the median is within one block of the configured pipeline"
            } else {
                "OUTSIDE one block of the configured pipeline"
            }
        );
    }
    if c.host_noise_ms >= 4.0 {
        let _ = writeln!(
            s,
            "  note: this machine was busy (a 1 ms sleep woke up to {:.1} ms late), so the numbers are rough",
            c.host_noise_ms
        );
    }
    let _ = writeln!(s, "  covers: {}", c.scope);
    s
}
// !SECTION: Probe

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_handles_a_flat_run() {
        let h = histogram(&[1.0, 1.0, 1.0]);
        assert_eq!(h.lines().count(), 1);
        assert!(h.contains('#') && h.trim_end().ends_with('3'));
    }

    #[test]
    fn histogram_spreads_a_range() {
        let samples: Vec<f32> = (0..20).map(|i| 1.0 + i as f32 * 0.1).collect();
        let h = histogram(&samples);
        assert_eq!(h.lines().count(), 8);
        let total: usize = h
            .lines()
            .map(|l| l.rsplit(' ').next().unwrap().parse::<usize>().unwrap())
            .sum();
        assert_eq!(total, 20, "every sample lands in a bar");
        assert_eq!(histogram(&[]), "");
        assert_eq!(histogram(&[f32::NAN]), "");
    }

    #[test]
    fn a_probe_with_no_answers_says_so() {
        let text = probe(&ProbeReport {
            rtt: Some(RttReport {
                sent: 3,
                received: 0,
                min_ms: 0.0,
                p50_ms: 0.0,
                p99_ms: 0.0,
                max_ms: 0.0,
                jitter_ms: 0.0,
                samples_ms: vec![],
            }),
            click_loop: None,
            notes: vec!["x".into()],
        });
        assert!(text.contains("0/3") && text.contains("no answer") && text.contains("note: x"));
        assert_eq!(probe(&ProbeReport::default()), "nothing was measured\n");
    }

    #[test]
    fn silence_is_printed_as_minus_inf() {
        assert_eq!(db(f32::NEG_INFINITY), "-inf");
        assert_eq!(db(-200.0), "-inf");
        assert_eq!(db(-6.04), "-6.0");
    }

    #[test]
    fn probe_wait_describes_the_work() {
        let req = ProbeRequest::default();
        assert!(probe_wait(&req).contains("10 clicks"));
        let req = ProbeRequest {
            peer: Some("10.0.0.2:6980".parse().unwrap()),
            click_loop: false,
            ..ProbeRequest::default()
        };
        assert!(probe_wait(&req).contains("10.0.0.2:6980"));
    }
}

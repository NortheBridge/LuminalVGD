// SPDX-License-Identifier: AGPL-3.0-only
//! vgd-probe — exercise the LuminalVGD control device end to end.
//!
//! Default run: open → handshake → CREATE_MONITOR → GET_STATUS →
//! QUERY_LEASE → ping/hold → DESTROY_MONITOR, narrating each step. The
//! hold window keeps the lease alive so the monitor is visible in
//! Display Settings.
//!
//! ```text
//! vgd-probe [status]              driver status only, no monitor
//! vgd-probe [WxH@HZ] [--hold N]   full cycle; default mode list is
//!                                 4K120 preferred + 4K60/1080p60
//!                                 fallbacks (15 s hold)
//! ```

#![cfg(windows)]

use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use luminal_driver_proto::{
    create_flags, err, ring_state, CreateMonitorRequest, GetStatusReply, ModeSpec,
    LEASE_TIMEOUT_USE_DEFAULT, MAX_MODES_PER_MONITOR,
};
use luminal_vgd_host::device::{RingView, VgdDevice};

/// Stable default display identity so repeated probe runs exercise the
/// identity-retention path (same connector, remembered settings).
const PROBE_DISPLAY_ID: u64 = 0x4C56_4744_0000_0001; // "LVGD" + 1

fn utf16_str(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

fn print_status(s: &GetStatusReply) {
    println!(
        "  driver build {} proto {}.{} caps {:#06x} max_monitors {} watchdog {}s uptime {} ms",
        s.driver_build, s.proto_major, s.proto_minor, s.caps, s.max_monitors, s.watchdog_secs,
        s.uptime_ms
    );
    if s.monitor_count == 0 {
        println!("  no monitors");
    }
    for m in &s.monitors[..s.monitor_count.min(s.monitors.len() as u32) as usize] {
        println!(
            "  session {:#x} display {:#x} connector {} {}x{}@{}mHz adapter {:#x} lease {} ms",
            m.session_id, m.display_id, m.connector_index, m.width, m.height, m.refresh_millihz,
            m.adapter_luid, m.lease_timeout_ms
        );
    }
}

struct Args {
    status_only: bool,
    /// Explicit `WxH@HZ` from the command line; None = default mode list.
    explicit_modes: Vec<ModeSpec>,
    hold_secs: u64,
    /// Mint a throwaway identity: Windows treats the monitor as brand new
    /// (no remembered topology/disconnect state) — useful when the stable
    /// probe identity has accumulated unwanted display-settings memory.
    ephemeral: bool,
    /// Act as a ring consumer during the hold: claim/release published
    /// slots continuously (~5 ms cadence). Exercises the reader protocol
    /// end to end; with a consumer draining, driver drops should stay ≈0.
    consume: bool,
}

/// Default mode list when none is given: 4K120 preferred (LG-OLED-class
/// clients are the LuminalShine baseline), with 4K60 and 1080p60
/// fallbacks. Also exercises the driver's MULTI_MODE path — the probe's
/// production counterpart passes the client's exact modes instead.
const DEFAULT_MODES: [ModeSpec; 3] = [
    ModeSpec { width: 3840, height: 2160, refresh_millihz: 120_000 },
    ModeSpec { width: 3840, height: 2160, refresh_millihz: 60_000 },
    ModeSpec { width: 1920, height: 1080, refresh_millihz: 60_000 },
];

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        status_only: false,
        explicit_modes: Vec::new(),
        hold_secs: 15,
        ephemeral: false,
        consume: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "status" => args.status_only = true,
            "--ephemeral" => args.ephemeral = true,
            "--consume" => args.consume = true,
            "--hold" => {
                let v = it.next().ok_or("--hold needs a value")?;
                args.hold_secs = v.parse().map_err(|_| format!("bad --hold value: {v}"))?;
            }
            mode if mode.contains('x') => {
                // WxH or WxH@HZ (HZ may be fractional, e.g. 59.94)
                let (dims, hz) = mode.split_once('@').unwrap_or((mode, "60"));
                let (w, h) = dims.split_once('x').ok_or_else(|| format!("bad mode: {mode}"))?;
                let hz: f64 = hz.parse().map_err(|_| format!("bad refresh: {hz}"))?;
                args.explicit_modes.push(ModeSpec {
                    width: w.parse().map_err(|_| format!("bad width: {w}"))?,
                    height: h.parse().map_err(|_| format!("bad height: {h}"))?,
                    refresh_millihz: (hz * 1000.0).round() as u32,
                });
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(args)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("vgd-probe: {e}");
            eprintln!("usage: vgd-probe [status] [WxH@HZ ...] [--hold SECS]");
            return ExitCode::FAILURE;
        }
    };

    println!("[1/6] opening LuminalVGD control device…");
    let dev = match VgdDevice::open_first() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("  cannot open device: {e}");
            eprintln!("  (is the driver installed? check Device Manager → Display adapters)");
            return ExitCode::FAILURE;
        }
    };

    println!("[2/6] handshake…");
    let hs = match dev.handshake() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("  handshake failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!(
        "  proto {}.{} build {} caps {:#06x} max_monitors {} watchdog {}s",
        hs.driver_proto_major, hs.driver_proto_minor, hs.driver_build, hs.caps, hs.max_monitors,
        hs.watchdog_secs
    );

    if args.status_only {
        println!("[3/6] GET_STATUS…");
        match dev.get_status() {
            Ok(s) => print_status(&s),
            Err(e) => eprintln!("  failed: {e}"),
        }
        return ExitCode::SUCCESS;
    }

    // Lease id unique per run; identity stable across runs.
    let session_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(1)
        | (std::process::id() as u64) << 48;

    let mode_list: Vec<ModeSpec> = if args.explicit_modes.is_empty() {
        DEFAULT_MODES.to_vec()
    } else {
        args.explicit_modes.clone()
    };
    let described: Vec<String> = mode_list
        .iter()
        .map(|m| format!("{}x{}@{}mHz", m.width, m.height, m.refresh_millihz))
        .collect();
    println!(
        "[3/6] CREATE_MONITOR [{}] (session {:#x}, display {:#x})…",
        described.join(", "),
        session_id,
        PROBE_DISPLAY_ID
    );
    let mut modes = [ModeSpec::default(); MAX_MODES_PER_MONITOR as usize];
    modes[..mode_list.len()].copy_from_slice(&mode_list);
    let mut friendly_name = [0u16; 32];
    for (i, c) in "LuminalVGD Probe".encode_utf16().enumerate() {
        friendly_name[i] = c;
    }
    let req = CreateMonitorRequest {
        session_id,
        display_id: if args.ephemeral { 0 } else { PROBE_DISPLAY_ID },
        adapter_luid: 0,
        lease_timeout_ms: LEASE_TIMEOUT_USE_DEFAULT,
        bit_depth: 8,
        hdr: 0,
        edid_serial: 0,
        flags: if args.ephemeral { create_flags::EPHEMERAL_IDENTITY } else { 0 },
        mode_count: mode_list.len() as u32,
        modes,
        physical_width_mm: 0,
        physical_height_mm: 0,
        friendly_name,
    };
    let reply = match dev.create_monitor(&req) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("  CREATE_MONITOR ioctl failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    if reply.result != err::OK {
        eprintln!("  driver refused: result {}", reply.result);
        return ExitCode::FAILURE;
    }
    println!(
        "  ok: connector {} ring_slots {} ring section {}",
        reply.connector_index,
        reply.ring_slots,
        utf16_str(&reply.ring_section_name)
    );

    println!("[4/6] GET_STATUS…");
    match dev.get_status() {
        Ok(s) => print_status(&s),
        Err(e) => eprintln!("  failed: {e}"),
    }
    match dev.query_lease(session_id) {
        Ok(l) if l.result == err::OK => {
            println!("  lease: {} ms remaining, connector {}", l.remaining_ms, l.connector_index)
        }
        Ok(l) => eprintln!("  QUERY_LEASE result {}", l.result),
        Err(e) => eprintln!("  QUERY_LEASE failed: {e}"),
    }

    println!(
        "[5/6] holding for {} s — monitor visible in Display Settings; watching the frame ring…",
        args.hold_secs
    );
    // The driver creates the ring section at monitor plug; allow a short
    // grace for the first map.
    let ring = {
        let mut view = None;
        for _ in 0..15 {
            match RingView::open(session_id, reply.ring_slots) {
                Ok(v) => {
                    view = Some(v);
                    break;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(200)),
            }
        }
        view
    };
    if ring.is_none() {
        eprintln!("  (ring section not mappable — transport inactive, control plane only)");
    }

    let mut first_seq = None;
    let mut prev_seq = 0u64;
    let mut prev_heartbeat = 0u64;
    let mut consumed_total = 0u64;
    for tick in 0..args.hold_secs {
        if args.consume {
            // Drain published slots at ~5 ms cadence for the whole tick:
            // claim newest → (a real consumer would encode here) → release.
            let tick_end = std::time::Instant::now() + Duration::from_secs(1);
            while std::time::Instant::now() < tick_end {
                if let Some(view) = &ring {
                    while let Some(frame) = view.claim_latest() {
                        view.release(frame.index);
                        consumed_total += 1;
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        } else {
            std::thread::sleep(Duration::from_secs(1));
        }
        match dev.ping(session_id) {
            Ok(r) if r == err::OK => {}
            Ok(r) => {
                eprintln!("  PING result {r} — lease lost?");
                break;
            }
            Err(e) => {
                eprintln!("  PING failed: {e}");
                break;
            }
        }
        if let Some(view) = &ring {
            let h = view.header();
            let state = match h.state {
                ring_state::ACTIVE => "ACTIVE",
                ring_state::REBUILDING => "REBUILDING",
                ring_state::DEAD => "DEAD",
                _ => "UNINITIALIZED",
            };
            let fps = h.latest_sequence.saturating_sub(prev_seq);
            let beating = if h.driver_heartbeat_qpc != prev_heartbeat { "beating" } else { "STALE" };
            let consumed = if args.consume {
                format!(" consumed {consumed_total}")
            } else {
                String::new()
            };
            println!(
                "  [{:>2}s] gen {} {} seq {} (+{}/s) published {} dropped {}{} heartbeat {}",
                tick + 1,
                h.ring_generation,
                state,
                h.latest_sequence,
                fps,
                h.frames_published,
                h.frames_dropped,
                consumed,
                beating
            );
            if first_seq.is_none() {
                first_seq = Some(h.latest_sequence);
            }
            prev_seq = h.latest_sequence;
            prev_heartbeat = h.driver_heartbeat_qpc;
        }
    }
    if let (Some(view), Some(first)) = (&ring, first_seq) {
        let advanced = view.header().latest_sequence.saturating_sub(first);
        if advanced > 0 {
            println!("  ring milestone: sequences advanced by {advanced} during the hold ✔");
        } else {
            println!("  ring milestone NOT met: latest_sequence did not advance ✘");
        }
    }

    println!("[6/6] DESTROY_MONITOR…");
    match dev.destroy_monitor(session_id) {
        Ok(r) if r == err::OK => println!("  ok — monitor should be gone again"),
        Ok(r) => eprintln!("  result {r}"),
        Err(e) => eprintln!("  failed: {e}"),
    }
    ExitCode::SUCCESS
}

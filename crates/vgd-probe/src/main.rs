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
//! vgd-probe [WxH@HZ] [--hold N]   full cycle (default 1920x1080@60, 15 s)
//! ```

#![cfg(windows)]

use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use luminal_driver_proto::{
    err, CreateMonitorRequest, GetStatusReply, ModeSpec, LEASE_TIMEOUT_USE_DEFAULT,
    MAX_MODES_PER_MONITOR,
};
use luminal_vgd_host::device::VgdDevice;

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
    width: u32,
    height: u32,
    refresh_millihz: u32,
    hold_secs: u64,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        status_only: false,
        width: 1920,
        height: 1080,
        refresh_millihz: 60_000,
        hold_secs: 15,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "status" => args.status_only = true,
            "--hold" => {
                let v = it.next().ok_or("--hold needs a value")?;
                args.hold_secs = v.parse().map_err(|_| format!("bad --hold value: {v}"))?;
            }
            mode if mode.contains('x') => {
                // WxH or WxH@HZ (HZ may be fractional, e.g. 59.94)
                let (dims, hz) = mode.split_once('@').unwrap_or((mode, "60"));
                let (w, h) = dims.split_once('x').ok_or_else(|| format!("bad mode: {mode}"))?;
                args.width = w.parse().map_err(|_| format!("bad width: {w}"))?;
                args.height = h.parse().map_err(|_| format!("bad height: {h}"))?;
                let hz: f64 = hz.parse().map_err(|_| format!("bad refresh: {hz}"))?;
                args.refresh_millihz = (hz * 1000.0).round() as u32;
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
            eprintln!("usage: vgd-probe [status] [WxH@HZ] [--hold SECS]");
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

    println!(
        "[3/6] CREATE_MONITOR {}x{}@{} mHz (session {:#x}, display {:#x})…",
        args.width, args.height, args.refresh_millihz, session_id, PROBE_DISPLAY_ID
    );
    let mut modes = [ModeSpec::default(); MAX_MODES_PER_MONITOR as usize];
    modes[0] = ModeSpec {
        width: args.width,
        height: args.height,
        refresh_millihz: args.refresh_millihz,
    };
    let mut friendly_name = [0u16; 32];
    for (i, c) in "LuminalVGD Probe".encode_utf16().enumerate() {
        friendly_name[i] = c;
    }
    let req = CreateMonitorRequest {
        session_id,
        display_id: PROBE_DISPLAY_ID,
        adapter_luid: 0,
        lease_timeout_ms: LEASE_TIMEOUT_USE_DEFAULT,
        bit_depth: 8,
        hdr: 0,
        edid_serial: 0,
        flags: 0,
        mode_count: 1,
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
        "[5/6] holding for {} s — the monitor should be visible in Display Settings now…",
        args.hold_secs
    );
    for _ in 0..args.hold_secs {
        std::thread::sleep(Duration::from_secs(1));
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
    }

    println!("[6/6] DESTROY_MONITOR…");
    match dev.destroy_monitor(session_id) {
        Ok(r) if r == err::OK => println!("  ok — monitor should be gone again"),
        Ok(r) => eprintln!("  result {r}"),
        Err(e) => eprintln!("  failed: {e}"),
    }
    ExitCode::SUCCESS
}

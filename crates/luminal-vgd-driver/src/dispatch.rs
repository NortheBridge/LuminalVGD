// SPDX-License-Identifier: AGPL-3.0-only
//! The control-device IOCTL dispatcher — the driver's entire control plane.
//!
//! The Windows shell's `EvtIoDeviceControl` does exactly this and nothing
//! more:
//!
//! ```text
//! let r = dispatch(&mut device_state, &mut handle_ctx, now_ms, code,
//!                  in_buf, out_buf);
//! match r.status { Ok => complete(r.bytes_written), BadBuffer => STATUS_INVALID_PARAMETER, ... }
//! for effect in r.effects { /* plug/unplug IddCx monitors, build rings */ }
//! ```
//!
//! Parsing, validation, session bookkeeping, and reply construction all
//! happen here, portably, under test. Buffers are treated as untrusted
//! bytes: too-short buffers are rejected, enum fields are range-checked in
//! core, and replies are fully written before success is returned.

use luminal_driver_proto::{
    err, ioctl, names, versions_compatible, CreateMonitorReply, CreateMonitorRequest,
    DestroyMonitorRequest, HandshakeRequest, PingRequest, ABI_MAX_RING_SLOTS,
    DEFAULT_RING_SLOTS,
};
use luminal_vgd_core::adapter::AdapterInfo;
use luminal_vgd_core::edid;
use luminal_vgd_core::modes::Mode;
use luminal_vgd_core::session::SessionTable;

/// Fixed driver-side configuration, read from the registry at device add
/// (SudoVDA kept its knobs in the registry; we keep only the global caps
/// there — per-monitor parameters travel in CREATE_MONITOR).
#[derive(Clone, Debug)]
pub struct DriverConfig {
    pub caps: u32,
    pub driver_build: u32,
    pub max_monitors: u32,
    pub watchdog_secs: u32,
    pub ring_slots: u32,
}

/// Per-device state owned by the shell, mutated only through dispatch.
pub struct DeviceState {
    pub table: SessionTable,
    cfg: DriverConfig,
    adapters: Vec<AdapterInfo>,
}

/// Per-open-handle context (one per host process handle). The handshake
/// gate is per-handle so a stale host from before a driver update can
/// never drive session IOCTLs with a mismatched idea of the ABI.
#[derive(Default)]
pub struct HandleCtx {
    pub handshaken: bool,
}

/// Side effects the shell must apply after a successful dispatch. The
/// dispatcher has already updated the session table; these carry what the
/// portable layer cannot do itself.
#[derive(Debug, PartialEq)]
pub enum Effect {
    /// Plug an IddCx monitor: serve `edid` from
    /// `EvtIddCxParseMonitorDescription`, advertise exactly `mode`, build
    /// the shared ring (`ring_slots` slots) on `adapter_luid`, section
    /// name per `names::ring_section_name(session_id)`.
    PlugMonitor {
        session_id: u64,
        mode: Mode,
        adapter_luid: u64,
        ring_slots: u32,
        edid: [u8; 128],
    },
    /// Unplug the monitor and free its ring (explicit destroy).
    UnplugMonitor { session_id: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Complete the IRP with this many output bytes.
    Ok,
    /// STATUS_INVALID_PARAMETER — buffer too small or unknown code.
    BadBuffer,
    /// Unknown IOCTL function (STATUS_INVALID_DEVICE_REQUEST).
    UnknownCode,
}

#[derive(Debug)]
pub struct DispatchResult {
    pub status: Status,
    pub bytes_written: usize,
    pub effects: Vec<Effect>,
}

impl DispatchResult {
    fn bad_buffer() -> Self {
        Self { status: Status::BadBuffer, bytes_written: 0, effects: Vec::new() }
    }
    fn ok(bytes_written: usize) -> Self {
        Self { status: Status::Ok, bytes_written, effects: Vec::new() }
    }
}

/// Read a `#[repr(C)]` request from an untrusted buffer. Larger buffers
/// are fine (forward compat: an older driver ignores new tail fields —
/// additive minor bumps rely on this); shorter are rejected.
fn read_req<T: Copy>(input: &[u8]) -> Option<T> {
    if input.len() < core::mem::size_of::<T>() {
        return None;
    }
    // Unaligned-safe: METHOD_BUFFERED gives us the SystemBuffer which is
    // aligned, but nothing in this signature promises that.
    Some(unsafe { core::ptr::read_unaligned(input.as_ptr().cast::<T>()) })
}

/// Write a full reply or nothing.
fn write_reply<T: Copy>(output: &mut [u8], reply: &T) -> Option<usize> {
    let n = core::mem::size_of::<T>();
    if output.len() < n {
        return None;
    }
    unsafe { core::ptr::copy_nonoverlapping((reply as *const T).cast::<u8>(), output.as_mut_ptr(), n) };
    Some(n)
}

impl DeviceState {
    pub fn new(cfg: DriverConfig) -> Self {
        Self {
            table: SessionTable::new(cfg.max_monitors, cfg.watchdog_secs),
            cfg,
            adapters: Vec::new(),
        }
    }

    /// Shell refreshes this on adapter arrival/departure notifications.
    pub fn set_adapters(&mut self, adapters: Vec<AdapterInfo>) {
        self.adapters = adapters;
    }

    fn ring_slots(&self) -> u32 {
        self.cfg.ring_slots.clamp(2, ABI_MAX_RING_SLOTS)
    }
}

impl Default for DriverConfig {
    fn default() -> Self {
        Self {
            caps: 0,
            driver_build: 0,
            max_monitors: luminal_driver_proto::DEFAULT_MAX_MONITORS,
            watchdog_secs: luminal_driver_proto::DEFAULT_WATCHDOG_SECS,
            ring_slots: DEFAULT_RING_SLOTS,
        }
    }
}

/// The one entry point for control IOCTLs.
pub fn dispatch(
    dev: &mut DeviceState,
    handle: &mut HandleCtx,
    now_ms: u64,
    code: u32,
    input: &[u8],
    output: &mut [u8],
) -> DispatchResult {
    match code {
        ioctl::IOCTL_HANDSHAKE => {
            let Some(req) = read_req::<HandshakeRequest>(input) else {
                return DispatchResult::bad_buffer();
            };
            let reply = dev.table.handshake_reply(dev.cfg.caps, dev.cfg.driver_build);
            // The reply always tells the truth about us; the gate opens
            // only for compatible hosts. (The host applies the same rule —
            // both sides refuse independently.)
            handle.handshaken = versions_compatible(
                req.host_proto_major,
                req.host_proto_minor,
                reply.driver_proto_major,
                reply.driver_proto_minor,
            );
            match write_reply(output, &reply) {
                Some(n) => DispatchResult::ok(n),
                None => DispatchResult::bad_buffer(),
            }
        }

        ioctl::IOCTL_CREATE_MONITOR => {
            let Some(req) = read_req::<CreateMonitorRequest>(input) else {
                return DispatchResult::bad_buffer();
            };
            let mut reply = CreateMonitorReply {
                session_id: req.session_id,
                result: err::OK,
                ring_slots: 0,
                ring_section_name: [0; 64],
            };
            let mut effects = Vec::new();
            if !handle.handshaken {
                reply.result = err::NOT_HANDSHAKEN;
            } else {
                let ring_slots = dev.ring_slots();
                match dev.table.create(now_ms, &req, dev.cfg.caps, &dev.adapters) {
                    Ok(monitor) => {
                        let monitor = monitor.clone();
                        reply.ring_slots = ring_slots;
                        names::ring_section_name(req.session_id, &mut reply.ring_section_name);
                        let edid_block =
                            edid::generate(&monitor.mode, &monitor.friendly_name, monitor.edid_serial);
                        effects.push(Effect::PlugMonitor {
                            session_id: monitor.session_id,
                            mode: monitor.mode,
                            adapter_luid: monitor.adapter_luid,
                            ring_slots,
                            edid: edid_block.bytes,
                        });
                    }
                    Err(e) => reply.result = e.code(),
                }
            }
            match write_reply(output, &reply) {
                Some(n) => DispatchResult { status: Status::Ok, bytes_written: n, effects },
                None => DispatchResult::bad_buffer(),
            }
        }

        ioctl::IOCTL_DESTROY_MONITOR => {
            let Some(req) = read_req::<DestroyMonitorRequest>(input) else {
                return DispatchResult::bad_buffer();
            };
            let (result, effects) = if !handle.handshaken {
                (err::NOT_HANDSHAKEN, Vec::new())
            } else {
                match dev.table.destroy(req.session_id) {
                    Ok(_) => (err::OK, vec![Effect::UnplugMonitor { session_id: req.session_id }]),
                    Err(e) => (e.code(), Vec::new()),
                }
            };
            match write_reply(output, &result) {
                Some(n) => DispatchResult { status: Status::Ok, bytes_written: n, effects },
                None => DispatchResult::bad_buffer(),
            }
        }

        ioctl::IOCTL_PING => {
            let Some(req) = read_req::<PingRequest>(input) else {
                return DispatchResult::bad_buffer();
            };
            let result = if !handle.handshaken {
                err::NOT_HANDSHAKEN
            } else {
                match dev.table.ping(now_ms, req.session_id) {
                    Ok(()) => err::OK,
                    Err(e) => e.code(),
                }
            };
            match write_reply(output, &result) {
                Some(n) => DispatchResult::ok(n),
                None => DispatchResult::bad_buffer(),
            }
        }

        ioctl::IOCTL_GET_STATUS => {
            // Diagnostics are deliberately available without a handshake —
            // the host's recovery ladder and support tooling use this to
            // tell "driver alive" from "driver gone" (DESIGN.md §3.3.4).
            let reply = dev.table.status(now_ms, dev.cfg.driver_build, dev.cfg.caps);
            match write_reply(output, &reply) {
                Some(n) => DispatchResult::ok(n),
                None => DispatchResult::bad_buffer(),
            }
        }

        _ => DispatchResult {
            status: Status::UnknownCode,
            bytes_written: 0,
            effects: Vec::new(),
        },
    }
}

/// Watchdog sweep, called from the shell's 1 s WDF timer. Returns unplug
/// effects for every monitor whose owner went silent.
pub fn watchdog_tick(dev: &mut DeviceState, now_ms: u64) -> Vec<Effect> {
    dev.table
        .tick(now_ms)
        .into_iter()
        .map(|m| Effect::UnplugMonitor { session_id: m.session_id })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use luminal_driver_proto::{
        caps, GetStatusReply, HandshakeReply, PROTO_VERSION_MAJOR, PROTO_VERSION_MINOR,
    };

    const CAPS: u32 = caps::HDR10 | caps::SDR10_BIT | caps::DIRTY_RECTS;

    fn dev() -> DeviceState {
        let mut d = DeviceState::new(DriverConfig {
            caps: CAPS,
            driver_build: 42,
            ..DriverConfig::default()
        });
        d.set_adapters(vec![AdapterInfo {
            luid: 0x20,
            vram_bytes: 16 << 30,
            name: "RTX 5080".into(),
            software: false,
        }]);
        d
    }

    fn as_bytes<T: Copy>(v: &T) -> Vec<u8> {
        let n = core::mem::size_of::<T>();
        let mut out = vec![0u8; n];
        unsafe { core::ptr::copy_nonoverlapping((v as *const T).cast::<u8>(), out.as_mut_ptr(), n) };
        out
    }

    fn from_bytes<T: Copy>(b: &[u8]) -> T {
        assert!(b.len() >= core::mem::size_of::<T>());
        unsafe { core::ptr::read_unaligned(b.as_ptr().cast::<T>()) }
    }

    fn shake(dev: &mut DeviceState, handle: &mut HandleCtx) {
        let req = HandshakeRequest {
            host_proto_major: PROTO_VERSION_MAJOR,
            host_proto_minor: PROTO_VERSION_MINOR,
        };
        let mut out = vec![0u8; core::mem::size_of::<HandshakeReply>()];
        let r = dispatch(dev, handle, 0, ioctl::IOCTL_HANDSHAKE, &as_bytes(&req), &mut out);
        assert_eq!(r.status, Status::Ok);
        assert!(handle.handshaken);
    }

    fn create_req(session_id: u64) -> CreateMonitorRequest {
        CreateMonitorRequest {
            session_id,
            adapter_luid: 0,
            width: 2560,
            height: 1440,
            refresh_millihz: 120_000,
            bit_depth: 8,
            hdr: 0,
            edid_serial: 7,
            flags: 0,
            reserved: 0,
            friendly_name: [0; 32],
        }
    }

    #[test]
    fn handshake_replies_and_gates() {
        let mut d = dev();
        let mut h = HandleCtx::default();
        let req = HandshakeRequest {
            host_proto_major: PROTO_VERSION_MAJOR,
            host_proto_minor: PROTO_VERSION_MINOR,
        };
        let mut out = vec![0u8; core::mem::size_of::<HandshakeReply>()];
        let r = dispatch(&mut d, &mut h, 0, ioctl::IOCTL_HANDSHAKE, &as_bytes(&req), &mut out);
        assert_eq!((r.status, r.bytes_written), (Status::Ok, out.len()));
        let reply: HandshakeReply = from_bytes(&out);
        assert_eq!(reply.driver_build, 42);
        assert_eq!(reply.caps, CAPS);
        assert!(h.handshaken);
    }

    #[test]
    fn major_mismatch_still_replies_but_gate_stays_shut() {
        let mut d = dev();
        let mut h = HandleCtx::default();
        let req = HandshakeRequest { host_proto_major: 999, host_proto_minor: 0 };
        let mut out = vec![0u8; core::mem::size_of::<HandshakeReply>()];
        let r = dispatch(&mut d, &mut h, 0, ioctl::IOCTL_HANDSHAKE, &as_bytes(&req), &mut out);
        assert_eq!(r.status, Status::Ok, "reply carries our version for the host's log");
        assert!(!h.handshaken);

        // Session IOCTLs on the unshaken handle answer NOT_HANDSHAKEN.
        let mut out = vec![0u8; core::mem::size_of::<CreateMonitorReply>()];
        let r = dispatch(
            &mut d,
            &mut h,
            0,
            ioctl::IOCTL_CREATE_MONITOR,
            &as_bytes(&create_req(1)),
            &mut out,
        );
        assert_eq!(r.status, Status::Ok);
        assert!(r.effects.is_empty());
        let reply: CreateMonitorReply = from_bytes(&out);
        assert_eq!(reply.result, err::NOT_HANDSHAKEN);
    }

    #[test]
    fn create_monitor_full_round_trip() {
        let mut d = dev();
        let mut h = HandleCtx::default();
        shake(&mut d, &mut h);

        let mut out = vec![0u8; core::mem::size_of::<CreateMonitorReply>()];
        let r = dispatch(
            &mut d,
            &mut h,
            1000,
            ioctl::IOCTL_CREATE_MONITOR,
            &as_bytes(&create_req(0xA1)),
            &mut out,
        );
        assert_eq!(r.status, Status::Ok);
        let reply: CreateMonitorReply = from_bytes(&out);
        assert_eq!(reply.result, err::OK);
        assert_eq!(reply.ring_slots, DEFAULT_RING_SLOTS);
        // Section name matches the shared naming helper exactly.
        let mut expect = [0u16; 64];
        names::ring_section_name(0xA1, &mut expect);
        assert_eq!(reply.ring_section_name, expect);

        // Effect tells the shell to plug the monitor with a valid EDID.
        assert_eq!(r.effects.len(), 1);
        match &r.effects[0] {
            Effect::PlugMonitor { session_id, mode, adapter_luid, ring_slots, edid } => {
                assert_eq!(*session_id, 0xA1);
                assert_eq!((mode.width, mode.height), (2560, 1440));
                assert_eq!(*adapter_luid, 0x20);
                assert_eq!(*ring_slots, DEFAULT_RING_SLOTS);
                let sum: u32 = edid.iter().map(|&b| u32::from(b)).sum();
                assert_eq!(sum % 256, 0, "EDID checksums");
            }
            other => panic!("unexpected effect {other:?}"),
        }
    }

    #[test]
    fn create_errors_travel_in_reply_not_status() {
        let mut d = dev();
        let mut h = HandleCtx::default();
        shake(&mut d, &mut h);

        let mut req = create_req(1);
        req.width = 9999; // out of envelope
        let mut out = vec![0u8; core::mem::size_of::<CreateMonitorReply>()];
        let r = dispatch(&mut d, &mut h, 0, ioctl::IOCTL_CREATE_MONITOR, &as_bytes(&req), &mut out);
        assert_eq!(r.status, Status::Ok, "IOCTL succeeds; the protocol error rides the reply");
        assert!(r.effects.is_empty());
        let reply: CreateMonitorReply = from_bytes(&out);
        assert_eq!(reply.result, err::BAD_MODE);
        assert_eq!(reply.ring_slots, 0);
    }

    #[test]
    fn destroy_and_ping_round_trip() {
        let mut d = dev();
        let mut h = HandleCtx::default();
        shake(&mut d, &mut h);
        let mut out = vec![0u8; core::mem::size_of::<CreateMonitorReply>()];
        dispatch(&mut d, &mut h, 0, ioctl::IOCTL_CREATE_MONITOR, &as_bytes(&create_req(5)), &mut out);

        // Ping known session: OK.
        let mut out4 = vec![0u8; 4];
        let r = dispatch(
            &mut d, &mut h, 500, ioctl::IOCTL_PING,
            &as_bytes(&PingRequest { session_id: 5 }), &mut out4,
        );
        assert_eq!((r.status, from_bytes::<i32>(&out4)), (Status::Ok, err::OK));

        // Ping unknown session: NO_SUCH_SESSION.
        let r = dispatch(
            &mut d, &mut h, 500, ioctl::IOCTL_PING,
            &as_bytes(&PingRequest { session_id: 6 }), &mut out4,
        );
        assert_eq!((r.status, from_bytes::<i32>(&out4)), (Status::Ok, err::NO_SUCH_SESSION));

        // Destroy: OK + unplug effect; second destroy: NO_SUCH_SESSION.
        let r = dispatch(
            &mut d, &mut h, 600, ioctl::IOCTL_DESTROY_MONITOR,
            &as_bytes(&DestroyMonitorRequest { session_id: 5 }), &mut out4,
        );
        assert_eq!(from_bytes::<i32>(&out4), err::OK);
        assert_eq!(r.effects, vec![Effect::UnplugMonitor { session_id: 5 }]);
        let r = dispatch(
            &mut d, &mut h, 700, ioctl::IOCTL_DESTROY_MONITOR,
            &as_bytes(&DestroyMonitorRequest { session_id: 5 }), &mut out4,
        );
        assert_eq!(from_bytes::<i32>(&out4), err::NO_SUCH_SESSION);
        assert!(r.effects.is_empty());
    }

    #[test]
    fn get_status_needs_no_handshake_and_reflects_state() {
        let mut d = dev();
        let mut shaken = HandleCtx::default();
        shake(&mut d, &mut shaken);
        let mut out = vec![0u8; core::mem::size_of::<CreateMonitorReply>()];
        dispatch(&mut d, &mut shaken, 0, ioctl::IOCTL_CREATE_MONITOR, &as_bytes(&create_req(9)), &mut out);

        // A brand-new, un-handshaken handle can still read diagnostics.
        let mut fresh = HandleCtx::default();
        let mut out = vec![0u8; core::mem::size_of::<GetStatusReply>()];
        let r = dispatch(&mut d, &mut fresh, 12345, ioctl::IOCTL_GET_STATUS, &[], &mut out);
        assert_eq!(r.status, Status::Ok);
        let s: GetStatusReply = from_bytes(&out);
        assert_eq!(s.monitor_count, 1);
        assert_eq!(s.monitors[0].session_id, 9);
        assert_eq!(s.uptime_ms, 12345);
        assert_eq!(s.driver_build, 42);
    }

    #[test]
    fn short_buffers_rejected_both_directions() {
        let mut d = dev();
        let mut h = HandleCtx::default();
        // Short input.
        let mut out = vec![0u8; core::mem::size_of::<HandshakeReply>()];
        let r = dispatch(&mut d, &mut h, 0, ioctl::IOCTL_HANDSHAKE, &[0u8; 2], &mut out);
        assert_eq!(r.status, Status::BadBuffer);
        assert!(!h.handshaken);
        // Short output.
        let req = HandshakeRequest {
            host_proto_major: PROTO_VERSION_MAJOR,
            host_proto_minor: PROTO_VERSION_MINOR,
        };
        let mut tiny = vec![0u8; 4];
        let r = dispatch(&mut d, &mut h, 0, ioctl::IOCTL_HANDSHAKE, &as_bytes(&req), &mut tiny);
        assert_eq!(r.status, Status::BadBuffer);
    }

    #[test]
    fn oversized_input_is_forward_compatible() {
        // A newer host may send a longer (minor-bumped) request; the tail
        // is ignored.
        let mut d = dev();
        let mut h = HandleCtx::default();
        let req = HandshakeRequest {
            host_proto_major: PROTO_VERSION_MAJOR,
            host_proto_minor: PROTO_VERSION_MINOR,
        };
        let mut input = as_bytes(&req);
        input.extend_from_slice(&[0xEE; 32]);
        let mut out = vec![0u8; core::mem::size_of::<HandshakeReply>()];
        let r = dispatch(&mut d, &mut h, 0, ioctl::IOCTL_HANDSHAKE, &input, &mut out);
        assert_eq!(r.status, Status::Ok);
        assert!(h.handshaken);
    }

    #[test]
    fn unknown_code_rejected() {
        let mut d = dev();
        let mut h = HandleCtx::default();
        let r = dispatch(&mut d, &mut h, 0, ioctl::ctl_code(0x8FF), &[], &mut []);
        assert_eq!(r.status, Status::UnknownCode);
    }

    #[test]
    fn watchdog_tick_emits_unplugs() {
        let mut d = dev();
        let mut h = HandleCtx::default();
        shake(&mut d, &mut h);
        let mut out = vec![0u8; core::mem::size_of::<CreateMonitorReply>()];
        dispatch(&mut d, &mut h, 0, ioctl::IOCTL_CREATE_MONITOR, &as_bytes(&create_req(1)), &mut out);
        dispatch(&mut d, &mut h, 0, ioctl::IOCTL_CREATE_MONITOR, &as_bytes(&create_req(2)), &mut out);

        // Session 1 pings at 2 s; session 2 never does. Default watchdog 3 s.
        let mut out4 = vec![0u8; 4];
        dispatch(&mut d, &mut h, 2_000, ioctl::IOCTL_PING, &as_bytes(&PingRequest { session_id: 1 }), &mut out4);

        assert!(watchdog_tick(&mut d, 3_000).is_empty());
        let reaped = watchdog_tick(&mut d, 3_100);
        assert_eq!(reaped, vec![Effect::UnplugMonitor { session_id: 2 }]);
        assert!(d.table.get(1).is_some());
    }
}

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
//! for effect in r.effects { /* plug/unplug IddCx monitors, rings, persist */ }
//! ```
//!
//! Parsing, validation, session bookkeeping, and reply construction all
//! happen here, portably, under test. Buffers are treated as untrusted
//! bytes: too-short buffers are rejected, enum fields are range-checked in
//! core, and replies are fully written before success is returned.

use luminal_driver_proto::{
    err, ioctl, names, versions_compatible, CreateMonitorReply, CreateMonitorRequest,
    DestroyMonitorRequest, HandshakeRequest, PermanentPoolConfig, PingRequest,
    QueryLeaseReply, QueryLeaseRequest, SetRenderAdapterRequest, ABI_MAX_RING_SLOTS,
    DEFAULT_RING_SLOTS,
};
use luminal_vgd_core::adapter::AdapterInfo;
use luminal_vgd_core::edid::{self, EdidParams};
use luminal_vgd_core::modes::Mode;
use luminal_vgd_core::permanent;
use luminal_vgd_core::persist::{self, PersistedState};
use luminal_vgd_core::session::{Monitor, SessionTable};

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
    /// Device-wide `SET_RENDER_ADAPTER` preference (0 = none).
    preferred_adapter: u64,
    /// Live permanent-pool config (`count == 0` = disbanded).
    pool: PermanentPoolConfig,
}

/// Per-open-handle context (one per host process handle). The handshake
/// gate is per-handle so a stale host from before a driver update can
/// never drive session IOCTLs with a mismatched idea of the ABI.
#[derive(Default)]
pub struct HandleCtx {
    pub handshaken: bool,
    /// DESIGN.md §6 control-surface ACL: set by the shell at file-create
    /// when the handle was opened through the control reference string by
    /// SYSTEM or an elevated Administrator. The shell refuses every IOCTL
    /// (including HANDSHAKE) on unauthorized handles before dispatch runs;
    /// the default is deny.
    pub authorized: bool,
}

/// Side effects the shell must apply after a successful dispatch. The
/// dispatcher has already updated the session table; these carry what the
/// portable layer cannot do itself.
#[derive(Debug, PartialEq)]
pub enum Effect {
    /// Plug an IddCx monitor on `connector_index`: serve `edid` from
    /// `EvtIddCxParseMonitorDescription`, advertise `modes`, build the
    /// shared ring (`ring_slots` slots) on `adapter_luid`, section names
    /// per `names::{ring,cursor}_section_name(session_id)`.
    PlugMonitor {
        session_id: u64,
        display_id: u64,
        connector_index: u32,
        modes: Vec<Mode>,
        adapter_luid: u64,
        ring_slots: u32,
        /// Boxed: keeps Effect variants near-uniform in size (effects
        /// travel by value through Vec<Effect>).
        edid: Box<[u8; 256]>,
    },
    /// Unplug the monitor and free its ring (explicit destroy, pool
    /// shrink, or watchdog reap).
    UnplugMonitor { session_id: u64 },
    /// Store this blob under the device registry key; hand it back to
    /// `DeviceState::new` on next start (identity retention + pool).
    PersistState(Vec<u8>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Complete the IRP with this many output bytes.
    Ok,
    /// STATUS_INVALID_PARAMETER — buffer too small.
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
    Some(unsafe { core::ptr::read_unaligned(input.as_ptr().cast::<T>()) })
}

/// Write a full reply or nothing.
fn write_reply<T: Copy>(output: &mut [u8], reply: &T) -> Option<usize> {
    let n = core::mem::size_of::<T>();
    if output.len() < n {
        return None;
    }
    unsafe {
        core::ptr::copy_nonoverlapping((reply as *const T).cast::<u8>(), output.as_mut_ptr(), n)
    };
    Some(n)
}

fn empty_pool() -> PermanentPoolConfig {
    PermanentPoolConfig {
        count: 0,
        width: 0,
        height: 0,
        refresh_millihz: 0,
        bit_depth: 0,
        hdr: 0,
        physical_width_mm: 0,
        physical_height_mm: 0,
        name: [0; 32],
    }
}

fn monitor_edid(m: &Monitor) -> [u8; 256] {
    edid::generate(&EdidParams {
        mode: m.preferred_mode(),
        friendly_name: &m.friendly_name,
        serial: m.edid_serial,
        product_code: m.product_code,
        physical_width_mm: m.physical_width_mm,
        physical_height_mm: m.physical_height_mm,
    })
    .bytes
}

fn plug_effect(m: &Monitor, ring_slots: u32) -> Effect {
    Effect::PlugMonitor {
        session_id: m.session_id,
        display_id: m.display_id,
        connector_index: m.connector_index,
        modes: m.modes.clone(),
        adapter_luid: m.adapter_luid,
        ring_slots,
        edid: Box::new(monitor_edid(m)),
    }
}

impl DeviceState {
    /// `persisted` is the blob from the last `Effect::PersistState` (or
    /// `None` on first install / corrupt state — parsing is defensive).
    pub fn new(cfg: DriverConfig, persisted: Option<&[u8]>) -> Self {
        let restored = persisted.and_then(persist::parse).unwrap_or_default();
        let mut table = SessionTable::new(cfg.max_monitors, cfg.watchdog_secs);
        table.restore_reservations(restored.reservations);
        Self {
            table,
            cfg,
            adapters: Vec::new(),
            preferred_adapter: 0,
            pool: restored.pool.unwrap_or_else(empty_pool),
        }
    }

    /// Shell refreshes this on adapter arrival/departure notifications.
    pub fn set_adapters(&mut self, adapters: Vec<AdapterInfo>) {
        self.adapters = adapters;
    }

    /// Recreate persisted permanent-pool members. Call once at device
    /// start, after `set_adapters`.
    pub fn startup(&mut self, now_ms: u64) -> Vec<Effect> {
        let mut effects = Vec::new();
        if self.pool.count > 0
            && permanent::validate(&self.pool, self.cfg.caps, self.table_cap()).is_ok()
        {
            let desired = self.pool;
            self.pool.count = 0;
            self.apply_pool(now_ms, &desired, &mut effects);
        }
        effects
    }

    fn table_cap(&self) -> u32 {
        self.cfg.max_monitors.min(luminal_driver_proto::ABI_MAX_MONITORS)
    }

    fn ring_slots(&self) -> u32 {
        self.cfg.ring_slots.clamp(2, ABI_MAX_RING_SLOTS)
    }

    fn persist_effect(&self) -> Effect {
        let pool = (self.pool.count > 0).then_some(self.pool);
        Effect::PersistState(persist::serialize(&PersistedState {
            reservations: self.table.reservations(),
            pool,
        }))
    }

    /// Destroy/create pool members to reach `desired`. Returns the proto
    /// result code; effects accumulate even on partial failure so the
    /// shell stays consistent with the table.
    fn apply_pool(
        &mut self,
        now_ms: u64,
        desired: &PermanentPoolConfig,
        effects: &mut Vec<Effect>,
    ) -> i32 {
        let plan = permanent::reconcile(&self.pool, self.pool.count, desired);
        for index in plan.destroy {
            let sid = permanent::permanent_session_id(index);
            if self.table.destroy(sid).is_ok() {
                effects.push(Effect::UnplugMonitor { session_id: sid });
            }
        }
        let mut result = err::OK;
        let mut created = if plan.create.is_empty() { desired.count } else { 0 };
        let ring_slots = self.ring_slots();
        for index in plan.create.iter().copied() {
            let req = permanent::member_request(desired, index);
            match self.table.create_trusted(now_ms, &req, self.cfg.caps, &self.adapters, self.preferred_adapter) {
                Ok(m) => {
                    let e = plug_effect(m, ring_slots);
                    effects.push(e);
                    created = index + 1;
                }
                Err(e) => {
                    result = e.code();
                    break;
                }
            }
        }
        self.pool = *desired;
        self.pool.count = created.min(desired.count);
        result
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
                display_id: 0,
                result: err::OK,
                ring_slots: 0,
                connector_index: 0,
                reserved: 0,
                ring_section_name: [0; 64],
            };
            let mut effects = Vec::new();
            if !handle.handshaken {
                reply.result = err::NOT_HANDSHAKEN;
            } else {
                let ring_slots = dev.ring_slots();
                match dev.table.create(
                    now_ms,
                    &req,
                    dev.cfg.caps,
                    &dev.adapters,
                    dev.preferred_adapter,
                ) {
                    Ok(monitor) => {
                        let monitor = monitor.clone();
                        reply.display_id = monitor.display_id;
                        reply.ring_slots = ring_slots;
                        reply.connector_index = monitor.connector_index;
                        names::ring_section_name(req.session_id, &mut reply.ring_section_name);
                        effects.push(plug_effect(&monitor, ring_slots));
                        effects.push(dev.persist_effect());
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
                    Ok(_) => (
                        err::OK,
                        vec![
                            Effect::UnplugMonitor { session_id: req.session_id },
                            dev.persist_effect(),
                        ],
                    ),
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

        ioctl::IOCTL_QUERY_LEASE => {
            let Some(req) = read_req::<QueryLeaseRequest>(input) else {
                return DispatchResult::bad_buffer();
            };
            let mut reply = QueryLeaseReply {
                session_id: req.session_id,
                display_id: 0,
                remaining_ms: 0,
                connector_index: 0,
                result: err::OK,
                reserved: 0,
            };
            if !handle.handshaken {
                reply.result = err::NOT_HANDSHAKEN;
            } else {
                match dev.table.query_lease(now_ms, req.session_id) {
                    Ok((display_id, connector, remaining)) => {
                        reply.display_id = display_id;
                        reply.connector_index = connector;
                        reply.remaining_ms = remaining;
                    }
                    Err(e) => reply.result = e.code(),
                }
            }
            match write_reply(output, &reply) {
                Some(n) => DispatchResult::ok(n),
                None => DispatchResult::bad_buffer(),
            }
        }

        ioctl::IOCTL_SET_RENDER_ADAPTER => {
            let Some(req) = read_req::<SetRenderAdapterRequest>(input) else {
                return DispatchResult::bad_buffer();
            };
            let result = if !handle.handshaken {
                err::NOT_HANDSHAKEN
            } else {
                dev.preferred_adapter = req.adapter_luid;
                err::OK
            };
            match write_reply(output, &result) {
                Some(n) => DispatchResult::ok(n),
                None => DispatchResult::bad_buffer(),
            }
        }

        ioctl::IOCTL_SET_PERMANENT_POOL => {
            let Some(req) = read_req::<PermanentPoolConfig>(input) else {
                return DispatchResult::bad_buffer();
            };
            let mut effects = Vec::new();
            let result = if !handle.handshaken {
                err::NOT_HANDSHAKEN
            } else {
                match permanent::validate(&req, dev.cfg.caps, dev.table_cap()) {
                    Ok(()) => {
                        let r = dev.apply_pool(now_ms, &req, &mut effects);
                        effects.push(dev.persist_effect());
                        r
                    }
                    Err(e) => e.code(),
                }
            };
            match write_reply(output, &result) {
                Some(n) => DispatchResult { status: Status::Ok, bytes_written: n, effects },
                None => DispatchResult::bad_buffer(),
            }
        }

        ioctl::IOCTL_QUERY_PERMANENT_POOL => {
            // Read-only, no handshake needed (diagnostics parity with
            // GET_STATUS).
            let reply = luminal_driver_proto::QueryPermanentPoolReply {
                config: dev.pool,
                result: err::OK,
                reserved: 0,
            };
            match write_reply(output, &reply) {
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
/// effects (plus a persist snapshot when anything was reaped).
pub fn watchdog_tick(dev: &mut DeviceState, now_ms: u64) -> Vec<Effect> {
    let reaped = dev.table.tick(now_ms);
    let mut effects: Vec<Effect> = reaped
        .iter()
        .map(|m| Effect::UnplugMonitor { session_id: m.session_id })
        .collect();
    if !effects.is_empty() {
        effects.push(dev.persist_effect());
    }
    effects
}

#[cfg(test)]
mod tests {
    use super::*;
    use luminal_driver_proto::{
        caps, GetStatusReply, HandshakeReply, ModeSpec, QueryPermanentPoolReply,
        LEASE_TIMEOUT_USE_DEFAULT, PROTO_VERSION_MAJOR, PROTO_VERSION_MINOR,
    };

    const CAPS: u32 =
        caps::HDR10 | caps::SDR10_BIT | caps::DIRTY_RECTS | caps::MULTI_MODE | caps::PERMANENT_POOL;

    fn dev() -> DeviceState {
        let mut d = DeviceState::new(
            DriverConfig { caps: CAPS, driver_build: 42, ..DriverConfig::default() },
            None,
        );
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
        let mut modes = [ModeSpec::default(); 4];
        modes[0] = ModeSpec { width: 2560, height: 1440, refresh_millihz: 120_000 };
        CreateMonitorRequest {
            session_id,
            display_id: 0,
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
            friendly_name: [0; 32],
        }
    }

    fn do_create(d: &mut DeviceState, h: &mut HandleCtx, req: &CreateMonitorRequest) -> (CreateMonitorReply, Vec<Effect>) {
        let mut out = vec![0u8; core::mem::size_of::<CreateMonitorReply>()];
        let r = dispatch(d, h, 1000, ioctl::IOCTL_CREATE_MONITOR, &as_bytes(req), &mut out);
        assert_eq!(r.status, Status::Ok);
        (from_bytes(&out), r.effects)
    }

    #[test]
    fn create_monitor_full_round_trip_with_identity_and_persist() {
        let mut d = dev();
        let mut h = HandleCtx::default();
        shake(&mut d, &mut h);

        let mut req = create_req(0xA1);
        req.display_id = 0xCAFE;
        let (reply, effects) = do_create(&mut d, &mut h, &req);
        assert_eq!(reply.result, err::OK);
        assert_eq!(reply.display_id, 0xCAFE);
        assert_eq!(reply.ring_slots, DEFAULT_RING_SLOTS);
        let mut expect = [0u16; 64];
        names::ring_section_name(0xA1, &mut expect);
        assert_eq!(reply.ring_section_name, expect);

        assert_eq!(effects.len(), 2, "plug + persist");
        match &effects[0] {
            Effect::PlugMonitor { session_id, display_id, connector_index, modes, adapter_luid, ring_slots, edid } => {
                assert_eq!((*session_id, *display_id), (0xA1, 0xCAFE));
                assert_eq!(*connector_index, 0);
                assert_eq!(modes.len(), 1);
                assert_eq!(*adapter_luid, 0x20);
                assert_eq!(*ring_slots, DEFAULT_RING_SLOTS);
                let base: u32 = edid[..128].iter().map(|&b| u32::from(b)).sum();
                let ext: u32 = edid[128..].iter().map(|&b| u32::from(b)).sum();
                assert_eq!((base % 256, ext % 256), (0, 0), "both EDID blocks checksum");
            }
            other => panic!("unexpected effect {other:?}"),
        }
        // Persist blob parses and carries the reservation.
        match &effects[1] {
            Effect::PersistState(blob) => {
                let state = persist::parse(blob).unwrap();
                assert_eq!(state.reservations, vec![(0xCAFE, 0)]);
            }
            other => panic!("unexpected effect {other:?}"),
        }
    }

    #[test]
    fn identity_survives_driver_restart_via_persist_blob() {
        let mut d = dev();
        let mut h = HandleCtx::default();
        shake(&mut d, &mut h);
        let mut req = create_req(1);
        req.display_id = 0xCAFE;
        let (reply, effects) = do_create(&mut d, &mut h, &req);
        assert_eq!(reply.connector_index, 0);
        let blob = match &effects[1] {
            Effect::PersistState(b) => b.clone(),
            _ => unreachable!(),
        };

        // "Restart": new DeviceState from the blob. Another identity
        // arrives first and must NOT take CAFE's connector.
        let mut d2 = DeviceState::new(
            DriverConfig { caps: CAPS, driver_build: 43, ..DriverConfig::default() },
            Some(&blob),
        );
        d2.set_adapters(vec![AdapterInfo {
            luid: 0x20,
            vram_bytes: 16 << 30,
            name: "RTX 5080".into(),
            software: false,
        }]);
        let mut h2 = HandleCtx::default();
        shake(&mut d2, &mut h2);
        let mut other = create_req(7);
        other.display_id = 0xBEEF;
        let (r_other, _) = do_create(&mut d2, &mut h2, &other);
        assert_eq!(r_other.connector_index, 1, "connector 0 reserved for CAFE");
        let mut back = create_req(8);
        back.display_id = 0xCAFE;
        let (r_back, _) = do_create(&mut d2, &mut h2, &back);
        assert_eq!(r_back.connector_index, 0);
    }

    #[test]
    fn set_render_adapter_steers_default_creates() {
        let mut d = dev();
        d.set_adapters(vec![
            AdapterInfo { luid: 0x10, vram_bytes: 8 << 30, name: "iGPU".into(), software: false },
            AdapterInfo { luid: 0x20, vram_bytes: 16 << 30, name: "dGPU".into(), software: false },
        ]);
        let mut h = HandleCtx::default();
        shake(&mut d, &mut h);

        let mut out4 = vec![0u8; 4];
        let r = dispatch(
            &mut d, &mut h, 0, ioctl::IOCTL_SET_RENDER_ADAPTER,
            &as_bytes(&SetRenderAdapterRequest { adapter_luid: 0x10 }), &mut out4,
        );
        assert_eq!((r.status, from_bytes::<i32>(&out4)), (Status::Ok, err::OK));

        let (_, effects) = do_create(&mut d, &mut h, &create_req(1));
        match &effects[0] {
            Effect::PlugMonitor { adapter_luid, .. } => assert_eq!(*adapter_luid, 0x10),
            other => panic!("unexpected effect {other:?}"),
        }
    }

    #[test]
    fn query_lease_round_trip() {
        let mut d = dev();
        let mut h = HandleCtx::default();
        shake(&mut d, &mut h);
        let mut req = create_req(5);
        req.lease_timeout_ms = 20_000;
        do_create(&mut d, &mut h, &req);

        let mut out = vec![0u8; core::mem::size_of::<QueryLeaseReply>()];
        let r = dispatch(
            &mut d, &mut h, 6_000, ioctl::IOCTL_QUERY_LEASE,
            &as_bytes(&QueryLeaseRequest { session_id: 5 }), &mut out,
        );
        assert_eq!(r.status, Status::Ok);
        let reply: QueryLeaseReply = from_bytes(&out);
        assert_eq!(reply.result, err::OK);
        assert_eq!(reply.remaining_ms, 15_000, "created at 1000, now 6000");

        let r = dispatch(
            &mut d, &mut h, 0, ioctl::IOCTL_QUERY_LEASE,
            &as_bytes(&QueryLeaseRequest { session_id: 99 }), &mut out,
        );
        assert_eq!(r.status, Status::Ok);
        assert_eq!(from_bytes::<QueryLeaseReply>(&out).result, err::NO_SUCH_SESSION);
    }

    #[test]
    fn permanent_pool_set_query_and_restart() {
        let mut d = dev();
        let mut h = HandleCtx::default();
        shake(&mut d, &mut h);

        let mut pool = PermanentPoolConfig {
            count: 2,
            width: 1920,
            height: 1080,
            refresh_millihz: 60_000,
            bit_depth: 8,
            hdr: 0,
            physical_width_mm: 0,
            physical_height_mm: 0,
            name: [0; 32],
        };
        let mut out4 = vec![0u8; 4];
        let r = dispatch(&mut d, &mut h, 0, ioctl::IOCTL_SET_PERMANENT_POOL, &as_bytes(&pool), &mut out4);
        assert_eq!(from_bytes::<i32>(&out4), err::OK);
        let plugs = r.effects.iter().filter(|e| matches!(e, Effect::PlugMonitor { .. })).count();
        assert_eq!(plugs, 2);
        assert_eq!(d.table.len(), 2);

        // Query reflects it (no handshake required).
        let mut fresh = HandleCtx::default();
        let mut out = vec![0u8; core::mem::size_of::<QueryPermanentPoolReply>()];
        dispatch(&mut d, &mut fresh, 0, ioctl::IOCTL_QUERY_PERMANENT_POOL, &[], &mut out);
        assert_eq!(from_bytes::<QueryPermanentPoolReply>(&out).config.count, 2);

        // Shrink to 1: one unplug.
        pool.count = 1;
        let r = dispatch(&mut d, &mut h, 0, ioctl::IOCTL_SET_PERMANENT_POOL, &as_bytes(&pool), &mut out4);
        let unplugs = r.effects.iter().filter(|e| matches!(e, Effect::UnplugMonitor { .. })).count();
        assert_eq!(unplugs, 1);
        assert_eq!(d.table.len(), 1);

        // Pool members never expire.
        assert!(watchdog_tick(&mut d, u64::MAX).is_empty());

        // "Reboot": restore from the persist blob and start up.
        let blob = match r.effects.last().unwrap() {
            Effect::PersistState(b) => b.clone(),
            _ => panic!("expected persist last"),
        };
        let mut d2 = DeviceState::new(
            DriverConfig { caps: CAPS, driver_build: 42, ..DriverConfig::default() },
            Some(&blob),
        );
        d2.set_adapters(vec![AdapterInfo {
            luid: 0x20, vram_bytes: 16 << 30, name: "RTX 5080".into(), software: false,
        }]);
        let effects = d2.startup(0);
        let plugs = effects.iter().filter(|e| matches!(e, Effect::PlugMonitor { .. })).count();
        assert_eq!(plugs, 1, "pool of 1 recreated at boot");
        assert_eq!(d2.table.len(), 1);
    }

    #[test]
    fn pool_validation_gates() {
        let mut d = dev();
        let mut h = HandleCtx::default();
        shake(&mut d, &mut h);
        let pool = PermanentPoolConfig {
            count: 5, // above MAX_PERMANENT_DISPLAYS
            width: 1920,
            height: 1080,
            refresh_millihz: 60_000,
            bit_depth: 8,
            hdr: 0,
            physical_width_mm: 0,
            physical_height_mm: 0,
            name: [0; 32],
        };
        let mut out4 = vec![0u8; 4];
        let r = dispatch(&mut d, &mut h, 0, ioctl::IOCTL_SET_PERMANENT_POOL, &as_bytes(&pool), &mut out4);
        assert_eq!(from_bytes::<i32>(&out4), err::BAD_POOL);
        assert!(r.effects.is_empty());
        assert!(d.table.is_empty());
    }

    #[test]
    fn gating_and_buffer_hygiene_hold_for_new_ioctls() {
        let mut d = dev();
        let mut un = HandleCtx::default(); // never handshaken
        let mut out4 = vec![0u8; 4];
        dispatch(
            &mut d, &mut un, 0, ioctl::IOCTL_SET_RENDER_ADAPTER,
            &as_bytes(&SetRenderAdapterRequest { adapter_luid: 1 }), &mut out4,
        );
        assert_eq!(from_bytes::<i32>(&out4), err::NOT_HANDSHAKEN);

        let mut out = vec![0u8; core::mem::size_of::<QueryLeaseReply>()];
        dispatch(
            &mut d, &mut un, 0, ioctl::IOCTL_QUERY_LEASE,
            &as_bytes(&QueryLeaseRequest { session_id: 1 }), &mut out,
        );
        assert_eq!(from_bytes::<QueryLeaseReply>(&out).result, err::NOT_HANDSHAKEN);

        // Short input/output buffers rejected.
        let r = dispatch(&mut d, &mut un, 0, ioctl::IOCTL_SET_PERMANENT_POOL, &[0u8; 4], &mut out4);
        assert_eq!(r.status, Status::BadBuffer);
        let mut tiny = vec![0u8; 2];
        let r = dispatch(
            &mut d, &mut un, 0, ioctl::IOCTL_QUERY_LEASE,
            &as_bytes(&QueryLeaseRequest { session_id: 1 }), &mut tiny,
        );
        assert_eq!(r.status, Status::BadBuffer);
    }

    #[test]
    fn destroy_ping_status_still_work_with_new_layout() {
        let mut d = dev();
        let mut h = HandleCtx::default();
        shake(&mut d, &mut h);
        do_create(&mut d, &mut h, &create_req(5));

        let mut out4 = vec![0u8; 4];
        dispatch(&mut d, &mut h, 500, ioctl::IOCTL_PING, &as_bytes(&PingRequest { session_id: 5 }), &mut out4);
        assert_eq!(from_bytes::<i32>(&out4), err::OK);

        let mut fresh = HandleCtx::default();
        let mut out = vec![0u8; core::mem::size_of::<GetStatusReply>()];
        dispatch(&mut d, &mut fresh, 12345, ioctl::IOCTL_GET_STATUS, &[], &mut out);
        let s: GetStatusReply = from_bytes(&out);
        assert_eq!(s.monitor_count, 1);
        assert_eq!(s.monitors[0].session_id, 5);
        assert_ne!(s.monitors[0].display_id, 0, "ephemeral identity derived");
        assert_eq!(s.monitors[0].lease_timeout_ms, 10_000);

        let r = dispatch(
            &mut d, &mut h, 600, ioctl::IOCTL_DESTROY_MONITOR,
            &as_bytes(&DestroyMonitorRequest { session_id: 5 }), &mut out4,
        );
        assert_eq!(from_bytes::<i32>(&out4), err::OK);
        assert_eq!(r.effects[0], Effect::UnplugMonitor { session_id: 5 });
        assert!(matches!(r.effects[1], Effect::PersistState(_)));
    }

    #[test]
    fn watchdog_reap_emits_unplug_and_persist() {
        let mut d = dev();
        let mut h = HandleCtx::default();
        shake(&mut d, &mut h);
        let mut req = create_req(1);
        req.lease_timeout_ms = 3_000;
        do_create(&mut d, &mut h, &req); // created at now=1000

        assert!(watchdog_tick(&mut d, 4_000).is_empty());
        let effects = watchdog_tick(&mut d, 4_001);
        assert_eq!(effects[0], Effect::UnplugMonitor { session_id: 1 });
        assert!(matches!(effects[1], Effect::PersistState(_)));
    }

    #[test]
    fn unknown_code_rejected() {
        let mut d = dev();
        let mut h = HandleCtx::default();
        let r = dispatch(&mut d, &mut h, 0, ioctl::ctl_code(0x8FF), &[], &mut []);
        assert_eq!(r.status, Status::UnknownCode);
    }
}

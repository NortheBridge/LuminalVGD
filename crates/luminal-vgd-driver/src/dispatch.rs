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
    err, ioctl, names, update_status, versions_compatible, CreateMonitorReply,
    CreateMonitorRequest, DestroyMonitorRequest, HandshakeRequest, PermanentPoolConfig,
    PingRequest, QueryLeaseReply, QueryLeaseRequest, SetRenderAdapterRequest,
    UpdateModesReply, UpdateModesRequest, ABI_MAX_RING_SLOTS, DEFAULT_RING_SLOTS,
    MAX_MODES_PER_MONITOR,
};
use luminal_vgd_core::adapter::AdapterInfo;
use luminal_vgd_core::edid::{self, EdidParams};
use luminal_vgd_core::modes::Mode;
use luminal_vgd_core::permanent;
use luminal_vgd_core::persist::{self, PersistedState};
use luminal_vgd_core::session::{Monitor, SessionTable};

/// TDR response policy: duck the DEVICE, keep the DISPLAY (build 16, the
/// default). On `DXGI_ERROR_DEVICE_REMOVED` the D3D device and swapchain
/// are torn down and the ring goes REBUILDING — but the IddCx monitor
/// stays ARRIVED and the OS display path stays alive, exactly as
/// DESIGN.md §3.3 rule 2 specifies ("Monitors stay attached"). Departure
/// becomes the gated fallback: only after OS recovery genuinely completes
/// (a requalify modeset against a healthy stack) or the long recovery
/// deadline expires.
pub const TDR_DUCK_DEVICE: u32 = 0;

/// TDR response policy: duck the DISPLAY (builds 14/15 behaviour, now
/// opt-in). Departs every monitor the instant a frame worker sees device
/// removal. Retained selectable because it is the only shipped-and-signed
/// TDR behaviour to date — but it AMPLIFIED the 2026-07-30 incident:
/// under `virtual_display_layout=exclusive` the departure took the active
/// display count to zero, DWM declared a black screen 131 ms later, and
/// the departure's `DBT_DEVNODES_CHANGED` broadcast is the documented
/// GTA V killer. Select with the `LuminalVgdTdrDuckMode` REG_DWORD under
/// the devnode's `Device Parameters` key (no reinstall; restart the
/// device to apply).
pub const TDR_DUCK_DISPLAY: u32 = 1;

/// Fixed driver-side configuration. `caps`/`driver_build` are compiled in;
/// `tdr_duck_mode` is the one knob read from the devnode registry at
/// device add (SudoVDA kept its knobs in the registry; per-monitor
/// parameters travel in CREATE_MONITOR instead).
#[derive(Clone, Debug)]
pub struct DriverConfig {
    pub caps: u32,
    pub driver_build: u32,
    pub max_monitors: u32,
    pub watchdog_secs: u32,
    pub ring_slots: u32,
    /// [`TDR_DUCK_DEVICE`] (default) or [`TDR_DUCK_DISPLAY`].
    pub tdr_duck_mode: u32,
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
    /// `EvtIddCxParseMonitorDescription`, describe the monitor with
    /// `modes`, publish `targets` as its IddCx target-mode list, build the
    /// shared ring (`ring_slots` slots) on `adapter_luid`, section names
    /// per `names::{ring,cursor}_section_name(session_id)`.
    PlugMonitor {
        session_id: u64,
        display_id: u64,
        connector_index: u32,
        /// The MONITOR-mode superset, which the EDID describes and which
        /// is frozen for the life of the monitor object.
        modes: Vec<Mode>,
        /// The TARGET subset to publish — the whole superset for a fresh
        /// session, or whatever `UPDATE_MODES` last put in force when the
        /// session is being replugged from `DeviceState`. Never empty,
        /// always a subset of `modes`.
        targets: Vec<Mode>,
        adapter_luid: u64,
        ring_slots: u32,
        /// Host-selected data-plane flags from `CreateMonitorRequest`.
        transport_flags: u32,
        /// Boxed: keeps Effect variants near-uniform in size (effects
        /// travel by value through Vec<Effect>).
        edid: Box<[u8; 256]>,
    },
    /// Unplug the monitor and free its ring (explicit destroy, pool
    /// shrink, or watchdog reap).
    UnplugMonitor { session_id: u64 },
    /// Replace the TARGET-mode list a LIVE monitor publishes (proto 0.5
    /// `UPDATE_MODES`) — no departure, no re-arrival, no ring churn.
    ///
    /// `targets` is the complete list to publish, already validated
    /// entry-by-entry against the monitor's create-time superset and
    /// guaranteed non-empty, so the shell never has to reason about what
    /// to keep. It exists as an Effect (rather than as work done inline
    /// in `dispatch`) for the same reason plug and unplug do: applying it
    /// calls `IddCxMonitorUpdateModes2`, which the OS answers by
    /// re-entering our mode DDIs synchronously, and that may only ever
    /// happen on the effects worker with no lock held (DESIGN.md §3.3
    /// rule 3).
    ///
    /// **The shell owes the table an answer.** The list is pending, not
    /// committed: whoever applies this effect MUST call
    /// `SessionTable::settle_modes(session_id, update_seq, …)` exactly
    /// once, with `Applied` only if `IddCxMonitorUpdateModes2` returned
    /// success. Without that call the durable list never changes and the
    /// host's next request re-pushes — safe, but a leak of intent; with a
    /// wrong `Applied` the durable state resumes lying, which is the
    /// defect this ordering exists to kill. `update_seq` is table-wide
    /// and monotonic, so a settle for a superseded or destroyed update
    /// no-ops instead of committing.
    UpdateModes { session_id: u64, update_seq: u64, targets: Vec<Mode> },
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

/// CREATE_MONITOR-specific reader: the other side of the additive-tail
/// contract. A 0.3 host sends the legacy 168-byte request; the appended
/// 0.4 fields (`max_nits`/`reserved0`) read as zeros — the documented
/// "driver default" semantics — so old hosts keep working against new
/// drivers with no version dance.
fn read_create_request(input: &[u8]) -> Option<CreateMonitorRequest> {
    if let Some(req) = read_req::<CreateMonitorRequest>(input) {
        return Some(req);
    }
    if input.len() < luminal_driver_proto::CREATE_MONITOR_REQUEST_SIZE_V3 {
        return None;
    }
    let mut padded = [0u8; core::mem::size_of::<CreateMonitorRequest>()];
    padded[..luminal_driver_proto::CREATE_MONITOR_REQUEST_SIZE_V3]
        .copy_from_slice(&input[..luminal_driver_proto::CREATE_MONITOR_REQUEST_SIZE_V3]);
    Some(unsafe { core::ptr::read_unaligned(padded.as_ptr().cast::<CreateMonitorRequest>()) })
}

/// UPDATE_MODES-specific reader (proto 0.5). Identical in shape to
/// [`read_create_request`] and present from the opcode's first day: the
/// full size first, then exactly one named legacy size with the tail
/// zero-padded. Today the two sizes are the same, so this is a plain
/// read — the point is that the NEXT minor which appends a field only has
/// to add a `UPDATE_MODES_REQUEST_SIZE_V<n>` constant and one branch,
/// instead of reconstructing the baseline after the fact the way
/// `CREATE_MONITOR_REQUEST_SIZE_V3` had to be.
fn read_update_modes_request(input: &[u8]) -> Option<UpdateModesRequest> {
    if let Some(req) = read_req::<UpdateModesRequest>(input) {
        return Some(req);
    }
    if input.len() < luminal_driver_proto::UPDATE_MODES_REQUEST_SIZE_V5 {
        return None;
    }
    let mut padded = [0u8; core::mem::size_of::<UpdateModesRequest>()];
    padded[..luminal_driver_proto::UPDATE_MODES_REQUEST_SIZE_V5]
        .copy_from_slice(&input[..luminal_driver_proto::UPDATE_MODES_REQUEST_SIZE_V5]);
    Some(unsafe { core::ptr::read_unaligned(padded.as_ptr().cast::<UpdateModesRequest>()) })
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
        max_nits: m.max_nits,
    })
    .bytes
}

fn plug_effect(m: &Monitor, ring_slots: u32) -> Effect {
    Effect::PlugMonitor {
        session_id: m.session_id,
        display_id: m.display_id,
        connector_index: m.connector_index,
        modes: m.modes.clone(),
        // The published subset travels with the plug: a replug that
        // reverted to the whole superset would silently undo whatever
        // gating an UPDATE_MODES had put in force, on a path (device
        // re-add, D3Final re-bring-up, pool restore) the host never sees.
        targets: m.target_modes.clone(),
        adapter_luid: m.adapter_luid,
        ring_slots,
        transport_flags: m.flags,
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

    /// The TDR response policy in force ([`TDR_DUCK_DEVICE`] or
    /// [`TDR_DUCK_DISPLAY`]).
    ///
    /// The shell mirrors this into `Shell::tdr_duck_mode` (an atomic) at
    /// device add, because the TDR path must never take the device lock —
    /// but it reads it FROM HERE, so the config field is the single
    /// source of truth rather than a value that merely happens to be
    /// carried alongside one. Build 16 shipped it write-only: the field
    /// was stored into `DriverConfig` and then never read by anything,
    /// while the shell mirrored a separate local. A later edit that fixed
    /// the config and forgot the local (or vice versa) would have silently
    /// shipped the wrong TDR policy — the one defect class the
    /// `TdrDuckConfig` event exists to make visible.
    pub fn tdr_duck_mode(&self) -> u32 {
        self.cfg.tdr_duck_mode
    }

    /// Reconcile portable state with a full runtime teardown (final
    /// device exit, or a discarded stale bring-up): every monitor's
    /// runtime is gone, so drop every table session — lease-disabled
    /// permanent-pool members included, which `tick()` can never reap —
    /// while the desired pool config stays so the next [`startup`]
    /// recreates its members. Skipping this bricks the pool on a
    /// same-process device re-add: startup's create_trusted hits
    /// DuplicateSession, creates zero members, and erases the desired
    /// count. Identity reservations survive per `destroy_all` semantics.
    pub fn device_teardown_reset(&mut self) {
        self.table.destroy_all();
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
            // Build 16 default: keep the display, duck only the device.
            // An absent registry value MUST mean this — see
            // shell::control::read_tdr_duck_mode.
            tdr_duck_mode: TDR_DUCK_DEVICE,
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
            let Some(req) = read_create_request(input) else {
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

        ioctl::IOCTL_UPDATE_MODES => {
            let Some(req) = read_update_modes_request(input) else {
                return DispatchResult::bad_buffer();
            };
            // Check the OUTPUT buffer before touching the table. Every
            // other opcode validates it only at write_reply time, which
            // leaves a caller with a short output buffer having mutated
            // the session while the effect that would have told the OS is
            // dropped with the BadBuffer result. Harmless for a create
            // (the session simply exists un-plugged and the watchdog
            // reaps it); NOT harmless here, where it would silently
            // diverge the durable list from the advertised one for the
            // rest of the session's life.
            if output.len() < core::mem::size_of::<UpdateModesReply>() {
                return DispatchResult::bad_buffer();
            }
            // `new` (not a struct literal) so the first-rejected word
            // starts at NO_REJECTED_INDEX — a literal zero there would
            // read back as "your FIRST mode was rejected".
            let mut reply = UpdateModesReply::new(req.session_id);
            let mut effects = Vec::new();
            // The published count in force on ANY outcome except "no such
            // session", so a host that is refused still learns what the
            // monitor is offering right now rather than guessing.
            let current = dev
                .table
                .get(req.session_id)
                .map(|m| m.target_modes.len() as u32)
                .unwrap_or(0);
            reply.mode_count = current;
            // Requested count is echoed on every outcome, including
            // refusals: `accepted < requested` is the caller's partial-
            // application test, and it must not read as "0 of 0" when the
            // request never got as far as the selection.
            reply.set_detail(0, req.mode_count, 0);
            if !handle.handshaken {
                reply.result = err::NOT_HANDSHAKEN;
            } else if req.mode_count == 0
                || req.mode_count > MAX_MODES_PER_MONITOR
                || req.mode_count as usize > req.modes.len()
            {
                // Bounds-check before slicing; `validate_list` would reject
                // these too, but a panic-free slice is not optional in a
                // driver reading an untrusted buffer. `mode_count == 0` is
                // also the wire-level half of IddCx.h:3594's
                // "TargetModeCount ... cannot be zero": the published list
                // may be replaced, never emptied.
                reply.result = err::BAD_MODE;
            } else {
                // `flags` is deliberately not validated: unknown bits are
                // IGNORED, matching create_flags, so a newer host cannot
                // be refused by an older driver over a bit it set. A
                // future flag that changes semantics ships with its own
                // caps bit instead.
                let specs = &req.modes[..req.mode_count as usize];
                // The commit token: "what has the OS committed" as a
                // version stamp. It decides whether a refusal this monitor
                // already collected is still evidence — see
                // `SessionTable::update_modes`. Read here, on the IOCTL
                // frame, because it is a single relaxed load of a global
                // the modeset callback bumps; nothing is locked and nothing
                // can block.
                let commit_token = crate::modepush::committed_token();
                match dev.table.update_modes(req.session_id, specs, dev.cfg.caps, commit_token) {
                    Ok(update) => {
                        reply.mode_count = update.targets.len() as u32;
                        reply.set_rejected(
                            update.rejected as u32,
                            update.first_rejected.map(|i| i as u32),
                        );
                        let mut flags = 0u32;
                        // PARTIAL: some requested modes have no entry in
                        // the monitor's create-time description, so they
                        // could never be offered no matter what is pushed.
                        // Reporting plain OK told a caller it could select
                        // a mode the monitor will never have — so say it,
                        // and say it without failing the request or
                        // throwing away the modes that DO exist
                        // (constraint 1).
                        if update.rejected > 0 {
                            flags |= update_status::PARTIAL;
                        }
                        // PENDING: the selection is NOT in force yet. The
                        // durable list is committed only when the shell
                        // reports the OS push succeeded
                        // (`SessionTable::settle_modes`), which cannot
                        // have happened before this IRP completes.
                        if update.pending {
                            flags |= update_status::PENDING;
                        }
                        reply.set_detail(update.accepted as u32, req.mode_count, flags);
                        if let Some(blocking_mode_idx) = update.blocked {
                            // PERMANENT refusal: this exact list was
                            // already refused because it would gate out the
                            // mode the OS has committed, and nothing has
                            // committed since. Say so on the wire — a
                            // distinct code AND the BLOCKED flag, plus the
                            // blocking mode's index in the list the host
                            // created the monitor with.
                            //
                            // Reported as its own outcome rather than as
                            // `UPDATE_FAILED`, which means "not applied,
                            // send it again": this one cannot succeed on a
                            // resend, and a host that resends it is in a
                            // loop with nothing on the other side. No
                            // effect is emitted — a second push would be
                            // refused by the same gate for the same reason.
                            // Constraint 1 holds throughout: the session,
                            // the monitor and the published list are all
                            // untouched.
                            reply.result = err::MODE_COMMITTED;
                            reply.set_blocked(blocking_mode_idx);
                        } else if update.refused {
                            // NOTHING requested exists on this monitor.
                            // Publishing the empty result would break
                            // IddCx.h:3594 and leave the monitor with no
                            // targets, so the request is refused with the
                            // rejection detail above and the published
                            // list is untouched. A refused REQUEST — the
                            // session, the monitor and the stream are all
                            // still running (constraint 1).
                            reply.result = err::BAD_MODE;
                        } else if let Some(update_seq) = update.queued {
                            // `queued` is Some only when THIS request
                            // changes what is published. A request that
                            // asks for the list already in force emits no
                            // effect: re-pushing an unchanged list is a
                            // modeset risk taken for nothing — but note
                            // that after a failed or deferred push the
                            // pending selection is discarded, so an
                            // identical retry lands here with a real
                            // change and does re-push. That is the whole
                            // build-17 commit-ordering fix.
                            effects.push(Effect::UpdateModes {
                                session_id: req.session_id,
                                update_seq,
                                targets: update.targets,
                            });
                        }
                        // No PersistState: the blob carries identity
                        // reservations and the pool config only — never
                        // mode lists — so there is nothing new to write.
                        // The published subset still survives an in-process
                        // replug because it lives in the session table,
                        // which is what `plug_effect` reads.
                    }
                    Err(e) => reply.result = e.code(),
                }
            }
            match write_reply(output, &reply) {
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
        UpdateModesReply, UpdateModesRequest, LEASE_TIMEOUT_USE_DEFAULT, PROTO_VERSION_MAJOR,
        PROTO_VERSION_MINOR,
    };
    use luminal_vgd_core::session::ModeUpdateResult;

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
            max_nits: 0,
            reserved0: 0,
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
            Effect::PlugMonitor { session_id, display_id, connector_index, modes, targets, adapter_luid, ring_slots, edid, transport_flags } => {
                assert_eq!((*session_id, *display_id), (0xA1, 0xCAFE));
                assert_eq!(*connector_index, 0);
                assert_eq!(modes.len(), 1);
                assert_eq!(targets, modes, "a fresh session publishes its whole superset");
                assert_eq!(*adapter_luid, 0x20);
                assert_eq!(*ring_slots, DEFAULT_RING_SLOTS);
                assert_eq!(*transport_flags, 0);
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
    fn legacy_v3_sized_create_request_is_accepted_with_default_nits() {
        // A proto-0.3 host sends the 168-byte CreateMonitorRequest (no
        // max_nits/reserved0 tail). The driver must accept it and treat
        // the missing tail as zeros — i.e. the default-luminance EDID.
        let mut d = dev();
        let mut h = HandleCtx::default();
        shake(&mut d, &mut h);
        let req = create_req(0xB2);
        let full = as_bytes(&req);
        let legacy = &full[..luminal_driver_proto::CREATE_MONITOR_REQUEST_SIZE_V3];
        let mut out = vec![0u8; core::mem::size_of::<CreateMonitorReply>()];
        let r = dispatch(&mut d, &mut h, 1000, ioctl::IOCTL_CREATE_MONITOR, legacy, &mut out);
        assert_eq!(r.status, Status::Ok);
        let reply: CreateMonitorReply = from_bytes(&out);
        assert_eq!(reply.result, err::OK);
        // And anything SHORTER than the legacy size stays rejected.
        let mut out2 = vec![0u8; core::mem::size_of::<CreateMonitorReply>()];
        let r2 = dispatch(
            &mut d,
            &mut h,
            1000,
            ioctl::IOCTL_CREATE_MONITOR,
            &full[..luminal_driver_proto::CREATE_MONITOR_REQUEST_SIZE_V3 - 4],
            &mut out2,
        );
        assert_eq!(r2.status, Status::BadBuffer);
    }

    #[test]
    fn create_request_nits_reaches_the_edid() {
        let mut d = dev();
        let mut h = HandleCtx::default();
        shake(&mut d, &mut h);
        let mut req = create_req(0xC3);
        req.bit_depth = 110;
        req.hdr = 1;
        req.max_nits = 800;
        let (reply, effects) = do_create(&mut d, &mut h, &req);
        assert_eq!(reply.result, err::OK);
        match &effects[0] {
            Effect::PlugMonitor { edid, .. } => {
                // CTA extension: colorimetry block at ext[4..8], HDR
                // metadata block at ext[8..15]; max-luminance code is
                // ext[12] (absolute byte 140). 800 nits = code 128.
                assert_eq!(edid[140], 128, "max luminance code for 800 nits");
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
    fn teardown_reset_preserves_pool_for_next_startup() {
        let mut d = dev();
        let mut h = HandleCtx::default();
        shake(&mut d, &mut h);

        let pool = PermanentPoolConfig {
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
        dispatch(&mut d, &mut h, 0, ioctl::IOCTL_SET_PERMANENT_POOL, &as_bytes(&pool), &mut out4);
        assert_eq!(from_bytes::<i32>(&out4), err::OK);
        assert_eq!(d.table.len(), 2);

        // Final device exit: runtime torn down, portable state reconciled.
        d.device_teardown_reset();
        assert_eq!(d.table.len(), 0, "lease-disabled pool members removed");

        // Replacement device: startup must recreate the full pool.
        // (Without the reset, create_trusted hits DuplicateSession,
        // creates zero members, and erases the desired count.)
        let effects = d.startup(1_000);
        let plugs = effects.iter().filter(|e| matches!(e, Effect::PlugMonitor { .. })).count();
        assert_eq!(plugs, 2, "pool recreated after teardown reset");
        assert_eq!(d.table.len(), 2);

        // And the cycle repeats: a second teardown + startup still works.
        d.device_teardown_reset();
        let effects = d.startup(2_000);
        let plugs = effects.iter().filter(|e| matches!(e, Effect::PlugMonitor { .. })).count();
        assert_eq!(plugs, 2);
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

    // -----------------------------------------------------------------
    // Build 17 / proto 0.5: UPDATE_MODES.
    // -----------------------------------------------------------------

    fn update_req(session_id: u64, specs: &[ModeSpec]) -> UpdateModesRequest {
        let mut modes = [ModeSpec::default(); 4];
        modes[..specs.len()].copy_from_slice(specs);
        UpdateModesRequest {
            session_id,
            flags: 0,
            mode_count: specs.len() as u32,
            modes,
            reserved: [0; 4],
        }
    }

    fn do_update(
        d: &mut DeviceState,
        h: &mut HandleCtx,
        req: &UpdateModesRequest,
    ) -> (UpdateModesReply, Vec<Effect>) {
        let mut out = vec![0u8; core::mem::size_of::<UpdateModesReply>()];
        let r = dispatch(d, h, 2000, ioctl::IOCTL_UPDATE_MODES, &as_bytes(req), &mut out);
        assert_eq!(r.status, Status::Ok);
        (from_bytes(&out), r.effects)
    }

    /// Take an `Effect::UpdateModes` out of a dispatch result: its
    /// `update_seq` (what the shell must settle) and the list it pushes.
    fn queued_update(effects: &[Effect]) -> Option<(u64, Vec<Mode>)> {
        effects.iter().find_map(|e| match e {
            Effect::UpdateModes { update_seq, targets, .. } => {
                Some((*update_seq, targets.clone()))
            }
            _ => None,
        })
    }

    const BASE_120: ModeSpec = ModeSpec { width: 2560, height: 1440, refresh_millihz: 120_000 };
    const FG_240: ModeSpec = ModeSpec { width: 2560, height: 1440, refresh_millihz: 240_000 };
    /// A rate deliberately NOT in `superset_req`'s create list: the mode
    /// this driver structurally cannot start offering on a live monitor.
    const NEVER_CREATED: ModeSpec =
        ModeSpec { width: 2560, height: 1440, refresh_millihz: 360_000 };

    /// Created with BOTH the base rate and the framegen-doubled rate in
    /// its monitor description — the shape a host must use if it wants to
    /// switch between them later, because the description is frozen at
    /// `IddCxMonitorCreate` and no IddCx DDI can extend it.
    fn superset_req(session_id: u64) -> CreateMonitorRequest {
        let mut r = create_req(session_id);
        r.mode_count = 2;
        r.modes[0] = BASE_120;
        r.modes[1] = FG_240;
        r
    }

    /// The build-17 milestone in one test: a monitor created with both
    /// rates is switched to publishing ONLY the framegen-doubled one on a
    /// LIVE session — one UpdateModes effect, no unplug, no plug, no
    /// monitor cycle anywhere — and then switched back.
    #[test]
    fn update_modes_republishes_a_subset_on_a_live_monitor() {
        let mut d = dev();
        let mut h = HandleCtx::default();
        shake(&mut d, &mut h);
        do_create(&mut d, &mut h, &superset_req(0xD4));
        assert_eq!(d.table.get(0xD4).unwrap().target_modes.len(), 2, "all of it, initially");

        let (reply, effects) = do_update(&mut d, &mut h, &update_req(0xD4, &[FG_240]));
        assert_eq!(reply.result, err::OK);
        assert_eq!(reply.session_id, 0xD4);
        assert_eq!(reply.mode_count, 1, "the subset that will be published");
        assert_eq!((reply.accepted(), reply.requested(), reply.rejected()), (1, 1, 0));
        assert_eq!(reply.first_rejected(), luminal_driver_proto::NO_REJECTED_INDEX);
        assert!(reply.is_pending(), "queued at the OS, not in force yet");
        assert!(!reply.is_partial());
        assert!(!reply.fully_in_force());

        assert_eq!(effects.len(), 1, "no plug, no unplug, no persist");
        let (seq, targets) = queued_update(&effects).expect("one push queued");
        match &effects[0] {
            Effect::UpdateModes { session_id, .. } => assert_eq!(*session_id, 0xD4),
            other => panic!("unexpected effect {other:?}"),
        }
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].refresh_millihz, 240_000);

        // Not durable yet: the IRP completed before the OS was told
        // anything, so a replug-from-DeviceState right now must still
        // carry the previously published list.
        assert_eq!(d.table.get(0xD4).unwrap().target_modes.len(), 2);

        // The shell reports the push succeeded — NOW it is durable.
        assert!(d.table.settle_modes(0xD4, seq, ModeUpdateResult::Applied, &targets).settled());
        let m = d.table.get(0xD4).unwrap();
        assert_eq!(m.target_modes.len(), 1);
        assert_eq!(m.modes.len(), 2, "the monitor description never moved");
        assert_eq!(m.preferred_mode().refresh_millihz, 120_000);

        // Resending the published list is a no-op: no effect at all, so
        // the OS is never asked to renegotiate for nothing — and the
        // reply says so without the PENDING flag.
        let (reply, effects) = do_update(&mut d, &mut h, &update_req(0xD4, &[FG_240]));
        assert_eq!((reply.result, reply.mode_count), (err::OK, 1));
        assert!(effects.is_empty());
        assert!(reply.fully_in_force(), "in force, in full, right now");

        // And back: gating is reversible within the superset, which is
        // the whole reason the target list rather than the monitor list
        // is what this opcode moves.
        let (reply, effects) = do_update(&mut d, &mut h, &update_req(0xD4, &[BASE_120, FG_240]));
        assert_eq!(reply.mode_count, 2);
        let (seq, targets) = queued_update(&effects).expect("a real change, so a real push");
        assert!(d.table.settle_modes(0xD4, seq, ModeUpdateResult::Applied, &targets).settled());
        assert_eq!(d.table.get(0xD4).unwrap().target_modes.len(), 2);
    }

    /// A target with no entry in the monitor's create-time description is
    /// REJECTED WITH DETAIL — never published, because Windows offers
    /// monitor∩target and it could not surface anyway.
    ///
    /// Constraint 1 is the point of the test: the request is refused, the
    /// SESSION is not. No effect, no departure, the previously published
    /// list still in force, and the reply carries enough detail (count and
    /// index) for a host to name the mode it cannot have.
    #[test]
    fn a_target_outside_the_superset_is_rejected_and_the_session_carries_on() {
        let mut d = dev();
        let mut h = HandleCtx::default();
        shake(&mut d, &mut h);
        do_create(&mut d, &mut h, &superset_req(0x5A));

        // PARTIAL: one of the two exists.
        let (reply, effects) =
            do_update(&mut d, &mut h, &update_req(0x5A, &[FG_240, NEVER_CREATED]));
        assert_eq!(reply.result, err::OK, "partial success is success, not an error");
        assert!(reply.is_partial(), "and it must not read as a clean OK");
        assert_eq!((reply.accepted(), reply.requested(), reply.rejected()), (1, 2, 1));
        assert_eq!(reply.first_rejected(), 1, "WHICH entry, not just how many");
        assert_eq!(reply.mode_count, 1);
        assert!(!reply.fully_in_force());
        let (seq, targets) = queued_update(&effects).expect("what exists is still published");
        assert_eq!(targets, vec![Mode::validate(2560, 1440, 240_000, 8, 0, CAPS).unwrap()]);
        assert!(d.table.settle_modes(0x5A, seq, ModeUpdateResult::Applied, &targets).settled());
        let published = d.table.get(0x5A).unwrap().target_modes.clone();
        assert_eq!(published.len(), 1);

        // WHOLLY outside: refused. The published list may never be
        // emptied — IddCx.h:3594 says TargetModeCount "cannot be zero" —
        // so nothing is pushed and nothing changes.
        let (reply, effects) = do_update(&mut d, &mut h, &update_req(0x5A, &[NEVER_CREATED]));
        assert_eq!(reply.result, err::BAD_MODE);
        assert_eq!((reply.accepted(), reply.requested(), reply.rejected()), (0, 1, 1));
        assert_eq!(reply.first_rejected(), 0, "index 0 is a real rejection, not 'none'");
        assert_eq!(reply.mode_count, 1, "what is still in force is reported");
        assert!(effects.is_empty(), "nothing may reach the OS");
        assert!(!reply.fully_in_force());

        // The session is untouched and entirely usable: still arrived,
        // still publishing a non-empty list, still able to take the next
        // request. A refused REQUEST is not a failed session.
        let m = d.table.get(0x5A).unwrap();
        assert_eq!(m.target_modes, published, "target modes unchanged, carry on");
        assert!(!m.target_modes.is_empty(), "never left with no targets");
        assert_eq!(m.modes.len(), 2, "and the description is still the description");
        assert!(d.table.pending_targets(0x5A).is_none(), "nothing queued");
        let (reply, effects) = do_update(&mut d, &mut h, &update_req(0x5A, &[BASE_120]));
        assert_eq!(reply.result, err::OK);
        assert!(queued_update(&effects).is_some(), "the next request works normally");
    }

    /// The header's other hard rule, at the wire: `TargetModeCount`
    /// "cannot be zero" (IddCx.h:3594). An empty request is refused before
    /// anything slices the array, emits no effect, and leaves the
    /// published list alone — the target list can be replaced, never
    /// emptied.
    #[test]
    fn an_empty_target_list_is_refused() {
        let mut d = dev();
        let mut h = HandleCtx::default();
        shake(&mut d, &mut h);
        do_create(&mut d, &mut h, &superset_req(0x1E));

        let mut empty = update_req(0x1E, &[FG_240]);
        empty.mode_count = 0;
        let (reply, effects) = do_update(&mut d, &mut h, &empty);
        assert_eq!(reply.result, err::BAD_MODE);
        assert_eq!(reply.mode_count, 2, "the list still in force");
        assert!(effects.is_empty(), "an empty push must never reach the OS");
        assert_eq!(d.table.get(0x1E).unwrap().target_modes.len(), 2);
        assert!(d.table.pending_targets(0x1E).is_none());

        // A zeroed request body (mode_count 0 with zeroed specs) is the
        // shape a buggy host is most likely to send; same answer.
        let zeroed = UpdateModesRequest {
            session_id: 0x1E,
            flags: 0,
            mode_count: 0,
            modes: [ModeSpec::default(); 4],
            reserved: [0; 4],
        };
        let (reply, effects) = do_update(&mut d, &mut h, &zeroed);
        assert_eq!(reply.result, err::BAD_MODE);
        assert!(effects.is_empty());
        assert_eq!(d.table.get(0x1E).unwrap().target_modes.len(), 2, "session fine");
    }

    /// FINDINGS 1/3/4 (the one defect from three angles): a push the OS
    /// REFUSED must leave the durable list where it was, and the
    /// identical retry must produce a real second push.
    ///
    /// Against the pre-fix code this test fails twice over: the durable
    /// list was committed at IOCTL time, so the assertion right after the
    /// failed settle finds the new list; and the retry resolved to
    /// "already published", emitted no effect, and reported plain OK — a
    /// silent no-op the caller cannot recover from, reporting success
    /// while the monitor offers the old list.
    #[test]
    fn a_retry_after_a_failed_push_really_pushes_again() {
        let mut d = dev();
        let mut h = HandleCtx::default();
        shake(&mut d, &mut h);
        do_create(&mut d, &mut h, &superset_req(7));

        let (reply, effects) = do_update(&mut d, &mut h, &update_req(7, &[FG_240]));
        assert!(reply.is_pending());
        let (seq, targets) = queued_update(&effects).expect("first request queues a push");

        // The OS refused it (monitors::update_modes rolled the runtime
        // list back and settles NotApplied).
        assert!(d.table.settle_modes(7, seq, ModeUpdateResult::NotApplied, &targets).settled());
        assert_eq!(
            d.table.get(7).unwrap().target_modes.len(),
            2,
            "the durable list must not claim a publish the OS refused"
        );
        assert_eq!(
            d.table.get(7).unwrap().last_error,
            err::UPDATE_FAILED,
            "and GET_STATUS says so"
        );

        // THE FIX: the identical retry pushes again.
        let (reply, effects) = do_update(&mut d, &mut h, &update_req(7, &[FG_240]));
        assert_eq!(reply.result, err::OK);
        let (retry_seq, retry_targets) =
            queued_update(&effects).expect("an identical retry must re-push, not no-op");
        assert_ne!(retry_seq, seq, "a NEW push, not a replay of the settled one");
        assert_eq!(retry_targets.len(), 1);
        assert!(
            reply.is_pending(),
            "and it must NOT read as in force while the publish has not happened"
        );
        assert!(!reply.fully_in_force());
        assert_eq!(d.table.get(7).unwrap().target_modes.len(), 2, "still not in force");

        // Second time lucky.
        assert!(d
            .table
            .settle_modes(7, retry_seq, ModeUpdateResult::Applied, &retry_targets)
            .settled());
        assert_eq!(d.table.get(7).unwrap().target_modes.len(), 1);
        let (reply, effects) = do_update(&mut d, &mut h, &update_req(7, &[FG_240]));
        assert!(effects.is_empty());
        assert!(reply.fully_in_force());
    }

    /// The same property for a DEFERRED push — the shell made no OS call
    /// at all (a TDR duck in flight, or the adapter torn down under a
    /// D3Final). A deferral is not an application: it settles NotApplied,
    /// so the durable list is untouched and the retry genuinely re-pushes
    /// once the deferring condition clears. Pre-fix, the durable list was
    /// already committed and the retry was a no-op reporting OK — the
    /// worst version of the bug, because a deferral is the case a host is
    /// most likely to hit and most likely to retry.
    #[test]
    fn a_retry_after_a_deferred_push_really_pushes_again() {
        let mut d = dev();
        let mut h = HandleCtx::default();
        shake(&mut d, &mut h);
        do_create(&mut d, &mut h, &superset_req(8));
        let before = d.table.get(8).unwrap().target_modes.clone();

        let (reply, effects) = do_update(&mut d, &mut h, &update_req(8, &[FG_240]));
        assert!(reply.is_pending());
        let (seq, targets) = queued_update(&effects).expect("first request queues a push");

        // Deferred: stored nowhere, pushed nowhere, retryable.
        assert!(d.table.settle_modes(8, seq, ModeUpdateResult::NotApplied, &targets).settled());
        assert_eq!(
            d.table.get(8).unwrap().target_modes,
            before,
            "target modes unchanged, carry on"
        );
        assert_eq!(d.table.get(8).unwrap().last_error, err::UPDATE_FAILED);

        let (reply, effects) = do_update(&mut d, &mut h, &update_req(8, &[FG_240]));
        let (retry_seq, retry_targets) =
            queued_update(&effects).expect("a retry after a deferral must re-push");
        assert_ne!(retry_seq, seq);
        assert!(reply.is_pending() && !reply.fully_in_force());
        assert!(d
            .table
            .settle_modes(8, retry_seq, ModeUpdateResult::Applied, &retry_targets)
            .settled());
        assert_eq!(d.table.get(8).unwrap().target_modes.len(), 1);
    }

    /// THE REGRESSION TEST for the retry loop, at the wire.
    ///
    /// A push refused because it would gate out the OS's COMMITTED mode is
    /// PERMANENT for as long as that mode stays committed. Build 17 settled
    /// it exactly like an OS-call failure — sticky `UPDATE_FAILED`, which
    /// the protocol documents as "the previous list is still in force and
    /// resending really does push again" — so a retrying host queued a
    /// push, had it refused for the same reason, was told to retry, and did,
    /// with no state anywhere converging.
    ///
    /// Against the pre-fix code this test fails on the retry, in the two
    /// places the loop is visible without any new API: the reply's `result`
    /// is `err::OK` (it was accepted for application) and an
    /// `Effect::UpdateModes` is emitted (it really is about to be pushed
    /// again). The reply carries no way to tell that from a transient
    /// failure, and no way to learn which mode is in the way.
    #[test]
    fn a_retry_after_a_committed_mode_refusal_is_answered_not_re_pushed() {
        use crate::modepush::{live_gate, CommittedMode, LiveGate, PushOutcome};

        let mut d = dev();
        let mut h = HandleCtx::default();
        shake(&mut d, &mut h);
        do_create(&mut d, &mut h, &superset_req(0xC0));
        let superset = d.table.get(0xC0).unwrap().modes.clone();
        // The OS has committed the BASE rate on this display: under
        // `virtual_display_layout=exclusive` it is the only active one.
        let committed = CommittedMode { width: 2560, height: 1440, refresh_millihz: 120_000 };

        // The host gates down to the framegen rate. Accepted and queued —
        // as it must be, the push has not happened yet.
        let (reply, effects) = do_update(&mut d, &mut h, &update_req(0xC0, &[FG_240]));
        assert_eq!(reply.result, err::OK);
        assert!(reply.is_pending() && !reply.is_blocked());
        assert_eq!(reply.blocking_mode_idx(), luminal_driver_proto::NO_MODE_INDEX);
        let (seq, targets) = queued_update(&effects).expect("first request queues a push");

        // The effects worker reaches the gate and refuses: publishing
        // {240} without the committed 120 would make Windows re-select
        // mid-stream on the only active display.
        let gate = live_gate(&superset, &targets, Some(committed));
        let LiveGate::EvictsCommitted { committed, superset_idx } = gate else {
            panic!("expected the committed-mode refusal, got {gate:?}");
        };
        let outcome = PushOutcome::Blocked {
            committed,
            superset_idx,
            token: crate::modepush::committed_token(),
            count: 1,
        };
        assert!(!outcome.retryable());
        assert!(d.table.settle_modes(0xC0, seq, outcome.settle_result(), &targets).settled());

        // Constraint 1: the PUSH was refused, never the session. The
        // published list is untouched and the sticky code is the distinct
        // one, so even a host that only polls GET_STATUS can tell.
        let m = d.table.get(0xC0).unwrap();
        assert_eq!(m.target_modes.len(), 2, "target modes unchanged, carry on");
        assert_eq!(m.last_error, err::MODE_COMMITTED);
        assert_ne!(err::MODE_COMMITTED, err::UPDATE_FAILED);

        // THE CONSEQUENCE, and the whole point: the identical retry.
        let (reply, effects) = do_update(&mut d, &mut h, &update_req(0xC0, &[FG_240]));
        assert_eq!(reply.result, err::MODE_COMMITTED, "not OK: this cannot be applied");
        assert!(reply.is_blocked(), "and the flag says the refusal is permanent");
        assert!(!reply.worth_retrying(), "which is the predicate a retry loop reads");
        assert!(
            effects.is_empty(),
            "and it is NOT pushed again — the same gate would refuse it identically"
        );
        // WHICH mode blocked it, as an index into the list this host used
        // at CREATE_MONITOR, so it can choose a different subset.
        assert_eq!(reply.blocking_mode_idx(), 0);
        assert_eq!(superset_req(0xC0).modes[reply.blocking_mode_idx() as usize], BASE_120);
        assert_eq!(reply.mode_count, 2, "the list still in force is reported as always");
        assert!(!reply.is_pending(), "nothing is outstanding — this is the final answer");
        assert!(!reply.fully_in_force());
        assert_eq!(d.table.get(0xC0).unwrap().target_modes.len(), 2);

        // The way out is open, and it is the one the reply pointed at: a
        // subset that KEEPS the blocking mode pushes normally.
        let (reply, effects) = do_update(&mut d, &mut h, &update_req(0xC0, &[BASE_120]));
        assert_eq!(reply.result, err::OK);
        assert!(!reply.is_blocked() && reply.worth_retrying());
        let (seq, targets) = queued_update(&effects).expect("a different subset really pushes");
        assert_eq!(live_gate(&superset, &targets, Some(committed)), LiveGate::Push);
        assert!(d.table.settle_modes(0xC0, seq, ModeUpdateResult::Applied, &targets).settled());
        assert_eq!(d.table.get(0xC0).unwrap().target_modes.len(), 1);
    }

    /// While a push is outstanding, a second request replaces the PENDING
    /// selection rather than the in-force one, and every reply keeps
    /// saying PENDING until something actually lands. The effects worker
    /// is serialized, so push #1 completes before push #2 starts, and the
    /// newer update is the one that decides what is FINALLY published —
    /// but a superseded push the OS ACCEPTED still records what the OS took
    /// on its way past, or the durable list under-reports the monitor until
    /// something else happens to move it.
    #[test]
    fn a_second_request_while_a_push_is_outstanding_supersedes_and_stays_pending() {
        let mut d = dev();
        let mut h = HandleCtx::default();
        shake(&mut d, &mut h);
        do_create(&mut d, &mut h, &superset_req(12));

        let (_, first) = do_update(&mut d, &mut h, &update_req(12, &[FG_240]));
        let (first_seq, first_targets) = queued_update(&first).unwrap();
        let (reply, second) = do_update(&mut d, &mut h, &update_req(12, &[BASE_120]));
        let (second_seq, second_targets) = queued_update(&second).unwrap();
        assert_eq!(second_targets.len(), 1);
        assert_eq!(second_targets[0].refresh_millihz, 120_000, "the newer intent wins");
        assert!(reply.is_pending());
        assert_eq!(d.table.pending_targets(12).unwrap().len(), 1);

        // The superseded push does not DECIDE — the pending selection is
        // still #2's — but the OS took its list, so the durable list has to
        // say so.
        let outcome =
            d.table.settle_modes(12, first_seq, ModeUpdateResult::Applied, &first_targets);
        assert!(!outcome.settled(), "it decided nothing for the outstanding update");
        assert!(outcome.recorded(), "but it recorded what the OS took");
        assert_eq!(d.table.get(12).unwrap().target_modes, first_targets);
        assert_eq!(d.table.pending_targets(12).unwrap().len(), 1, "#2 still outstanding");

        // And the newer one still has the last word.
        assert!(d
            .table
            .settle_modes(12, second_seq, ModeUpdateResult::Applied, &second_targets)
            .settled());
        assert_eq!(d.table.get(12).unwrap().target_modes, second_targets);

        // Resending the list now in force is a no-op that reads as such.
        let (reply, effects) = do_update(&mut d, &mut h, &update_req(12, &[BASE_120]));
        assert!(effects.is_empty());
        assert!(reply.fully_in_force());
    }

    /// THE REGRESSION TEST for the unreachable rescind, at the wire.
    ///
    /// The interleaving is real, not theoretical: `dispatch` holds the
    /// device lock only for its own duration, while the effects worker
    /// calls `IddCxMonitorUpdateModes2` with no lock held — so request #2's
    /// whole dispatch fits inside push #1's DDI call.
    ///
    /// Against the pre-fix code this fails on the LAST four assertions, and
    /// it fails in the two places a host can actually see: the rescind
    /// emits no `Effect::UpdateModes`, and its reply says
    /// `fully_in_force()` — "everything you asked for is offered right
    /// now" — for a list the OS is not holding. That reply is the predicate
    /// DESIGN.md tells hosts to gate client capability on.
    #[test]
    fn a_rescind_after_a_superseded_success_still_reaches_the_os() {
        let mut d = dev();
        let mut h = HandleCtx::default();
        shake(&mut d, &mut h);
        do_create(&mut d, &mut h, &superset_req(0x11));
        assert_eq!(d.table.get(0x11).unwrap().target_modes.len(), 2, "both rates, initially");

        // #1 gates down to the framegen rate; the effects worker is inside
        // IddCxMonitorUpdateModes2 with no lock held.
        let (_, first) = do_update(&mut d, &mut h, &update_req(0x11, &[FG_240]));
        let (first_seq, first_targets) = queued_update(&first).expect("#1 queues a push");

        // #2's whole dispatch lands inside that call.
        let (_, second) = do_update(&mut d, &mut h, &update_req(0x11, &[BASE_120]));
        let (second_seq, second_targets) = queued_update(&second).expect("#2 queues a push");

        // #1 returns STATUS_SUCCESS. Superseded — but the OS holds {240}.
        d.table.settle_modes(0x11, first_seq, ModeUpdateResult::Applied, &first_targets);
        // #2 does not apply: a duck in flight, a torn-down adapter, or an
        // OS refusal whose rollback restores #1's list. `UPDATE_FAILED` is
        // the sticky code, i.e. the host is told to retry.
        d.table.settle_modes(0x11, second_seq, ModeUpdateResult::NotApplied, &second_targets);
        assert_eq!(d.table.get(0x11).unwrap().last_error, err::UPDATE_FAILED);

        // THE CONSEQUENCE: the host rescinds back to the full list.
        let (reply, effects) = do_update(&mut d, &mut h, &update_req(0x11, &[BASE_120, FG_240]));
        assert_eq!(reply.result, err::OK);
        assert_eq!(reply.mode_count, 2);
        let (_, rescind_targets) =
            queued_update(&effects).expect("the rescind must reach the OS, not short-circuit");
        assert_eq!(rescind_targets.len(), 2);
        assert!(reply.is_pending(), "it is queued, not in force");
        assert!(
            !reply.fully_in_force(),
            "and must never claim in-force for a list the OS is not holding"
        );
    }

    /// Constraint 1: every refusal is a REPLY code with the published
    /// count intact — never a failed IRP, never an effect, never a
    /// disturbed session.
    #[test]
    fn update_modes_refusals_leave_the_session_alone() {
        let mut d = dev();
        let mut h = HandleCtx::default();
        shake(&mut d, &mut h);
        do_create(&mut d, &mut h, &superset_req(9));

        // Not handshaken: refused, and the live count still reported.
        let mut un = HandleCtx::default();
        let (reply, effects) = do_update(&mut d, &mut un, &update_req(9, &[FG_240]));
        assert_eq!(reply.result, err::NOT_HANDSHAKEN);
        assert_eq!(reply.mode_count, 2, "the list in force is still reported");
        assert!(effects.is_empty());
        // A refusal takes nothing and queues nothing, and says both.
        assert_eq!((reply.accepted(), reply.requested()), (0, 1));
        assert_eq!(reply.flags(), 0);
        assert!(!reply.fully_in_force(), "a refusal is never 'in force'");

        // Unknown session: 0 modes in force is the one case that means
        // "there is nothing there".
        let (reply, effects) = do_update(&mut d, &mut h, &update_req(1234, &[FG_240]));
        assert_eq!((reply.result, reply.mode_count), (err::NO_SUCH_SESSION, 0));
        assert!(effects.is_empty());

        // mode_count > MAX is bounds-checked before anything slices the
        // array (mode_count == 0 has its own test above).
        let mut too_many = update_req(9, &[FG_240]);
        too_many.mode_count = luminal_driver_proto::MAX_MODES_PER_MONITOR + 1;
        assert_eq!(do_update(&mut d, &mut h, &too_many).0.result, err::BAD_MODE);

        // Out-of-envelope mode.
        let bad = ModeSpec { width: 3, height: 3, refresh_millihz: 1 };
        let (reply, effects) = do_update(&mut d, &mut h, &update_req(9, &[bad]));
        assert_eq!((reply.result, reply.mode_count), (err::BAD_MODE, 2));
        assert!(effects.is_empty());

        // Short input and short output buffers stay BadBuffer.
        let full = as_bytes(&update_req(9, &[FG_240]));
        let mut out = vec![0u8; core::mem::size_of::<UpdateModesReply>()];
        let r = dispatch(
            &mut d, &mut h, 0, ioctl::IOCTL_UPDATE_MODES,
            &full[..luminal_driver_proto::UPDATE_MODES_REQUEST_SIZE_V5 - 4], &mut out,
        );
        assert_eq!(r.status, Status::BadBuffer);
        // A short OUTPUT buffer must be rejected BEFORE the table is
        // touched: the effect would be dropped with the BadBuffer result,
        // so a mutation here would leave the durable list permanently
        // ahead of what the monitor advertises.
        let mut tiny = vec![0u8; 8];
        let r = dispatch(&mut d, &mut h, 0, ioctl::IOCTL_UPDATE_MODES, &full, &mut tiny);
        assert_eq!(r.status, Status::BadBuffer);
        assert!(r.effects.is_empty());
        assert!(
            d.table.pending_targets(9).is_none(),
            "short reply buffer changed nothing"
        );

        // Through all of that, the monitor never changed.
        assert_eq!(d.table.get(9).unwrap().target_modes.len(), 2);
        assert_eq!(d.table.get(9).unwrap().modes.len(), 2);
    }

    /// FORWARD compatibility, request side: a FUTURE host that appends
    /// fields sends a LARGER buffer, and this driver must accept it and
    /// ignore the tail — that is the whole additive-growth contract, and
    /// it only works if the exact 0.5 size is also still accepted.
    #[test]
    fn update_modes_accepts_the_v5_size_and_a_future_larger_request() {
        let mut d = dev();
        let mut h = HandleCtx::default();
        shake(&mut d, &mut h);
        do_create(&mut d, &mut h, &superset_req(11));
        let full = as_bytes(&update_req(11, &[FG_240]));
        assert_eq!(full.len(), luminal_driver_proto::UPDATE_MODES_REQUEST_SIZE_V5);

        // Exactly the 0.5 size.
        let mut out = vec![0u8; core::mem::size_of::<UpdateModesReply>()];
        let r = dispatch(&mut d, &mut h, 0, ioctl::IOCTL_UPDATE_MODES, &full, &mut out);
        assert_eq!(r.status, Status::Ok);
        assert_eq!(from_bytes::<UpdateModesReply>(&out).result, err::OK);

        // A hypothetical 0.6 host: same prefix, 32 bytes of new tail.
        let mut d2 = dev();
        let mut h2 = HandleCtx::default();
        shake(&mut d2, &mut h2);
        do_create(&mut d2, &mut h2, &superset_req(11));
        let mut future = full.clone();
        future.extend_from_slice(&[0xAB; 32]);
        let mut out2 = vec![0u8; core::mem::size_of::<UpdateModesReply>()];
        let r = dispatch(&mut d2, &mut h2, 0, ioctl::IOCTL_UPDATE_MODES, &future, &mut out2);
        assert_eq!(r.status, Status::Ok);
        let reply: UpdateModesReply = from_bytes(&out2);
        assert_eq!((reply.result, reply.mode_count), (err::OK, 1));

        // Unknown flag bits are IGNORED, not refused — an older driver
        // must never reject a newer host over a bit it does not know.
        let mut flagged = update_req(11, &[BASE_120]);
        flagged.flags = 0xDEAD_BEEF;
        assert_eq!(do_update(&mut d2, &mut h2, &flagged).0.result, err::OK);
    }

    /// BACKWARD compatibility, the direction that matters most: a host
    /// built against proto 0.3/0.4 — which announces the required FLOOR,
    /// not its compiled minor — still handshakes against this 0.5 driver
    /// and drives every pre-0.5 opcode unchanged. It simply never sends
    /// 0x809.
    #[test]
    fn a_pre_05_host_is_completely_unaffected_by_the_new_opcode() {
        for announced in [3u16, 4u16] {
            let mut d = dev();
            let mut h = HandleCtx::default();
            let req = HandshakeRequest {
                host_proto_major: PROTO_VERSION_MAJOR,
                host_proto_minor: announced,
            };
            let mut out = vec![0u8; core::mem::size_of::<HandshakeReply>()];
            let r = dispatch(&mut d, &mut h, 0, ioctl::IOCTL_HANDSHAKE, &as_bytes(&req), &mut out);
            assert_eq!(r.status, Status::Ok);
            let reply: HandshakeReply = from_bytes(&out);
            assert_eq!(reply.driver_proto_minor, PROTO_VERSION_MINOR);
            assert_eq!(reply.driver_proto_minor, 8);
            assert!(h.handshaken, "0.{announced} host still handshakes against 0.8");

            // And its session IOCTLs still work.
            let (create, effects) = do_create(&mut d, &mut h, &create_req(3));
            assert_eq!(create.result, err::OK);
            assert!(matches!(effects[0], Effect::PlugMonitor { .. }));
        }
    }

    /// The structural backstop for the other direction: a build-17 HOST
    /// talking to an older driver. There is no way to simulate an old
    /// driver here, so the property under test is the one that makes the
    /// fallback safe — an unrecognized code produces `UnknownCode`
    /// (STATUS_INVALID_DEVICE_REQUEST → an I/O error host-side), never a
    /// zero-filled reply that could read as success.
    #[test]
    fn an_unknown_opcode_can_never_look_like_an_update_modes_success() {
        let mut d = dev();
        let mut h = HandleCtx::default();
        shake(&mut d, &mut h);
        let mut out = vec![0u8; core::mem::size_of::<UpdateModesReply>()];
        let r = dispatch(
            &mut d, &mut h, 0, ioctl::ctl_code(0x80A),
            &as_bytes(&update_req(1, &[ModeSpec { width: 1920, height: 1080, refresh_millihz: 60_000 }])),
            &mut out,
        );
        assert_eq!(r.status, Status::UnknownCode);
        assert_eq!(r.bytes_written, 0, "nothing written — no false success");
        assert!(r.effects.is_empty());
    }

    /// The caps bit is the host's feature gate, so it has to be ON in the
    /// handshake this driver actually returns — not merely defined.
    #[test]
    fn handshake_advertises_dynamic_modes_when_the_driver_has_it() {
        let mut d = DeviceState::new(
            DriverConfig {
                caps: CAPS | caps::DYNAMIC_MODES,
                driver_build: 17,
                ..DriverConfig::default()
            },
            None,
        );
        let mut h = HandleCtx::default();
        let mut out = vec![0u8; core::mem::size_of::<HandshakeReply>()];
        dispatch(
            &mut d, &mut h, 0, ioctl::IOCTL_HANDSHAKE,
            &as_bytes(&HandshakeRequest {
                host_proto_major: PROTO_VERSION_MAJOR,
                host_proto_minor: luminal_driver_proto::PROTO_VERSION_MINOR_REQUIRED,
            }),
            &mut out,
        );
        let reply: HandshakeReply = from_bytes(&out);
        assert_ne!(reply.caps & caps::DYNAMIC_MODES, 0);
        assert_eq!(reply.driver_build, 17);
    }

    #[test]
    fn unknown_code_rejected() {
        let mut d = dev();
        let mut h = HandleCtx::default();
        let r = dispatch(&mut d, &mut h, 0, ioctl::ctl_code(0x8FF), &[], &mut []);
        assert_eq!(r.status, Status::UnknownCode);
    }

    /// Build 16, constraint 1: the SHIPPED default must be "duck the
    /// device, keep the display" (DESIGN.md §3.3 rule 2), and the legacy
    /// build-14/15 display duck-out must remain SELECTABLE. A silent flip
    /// of this default is a behaviour change nobody would see in a diff of
    /// the shell — the 2026-07-30 incident is what it costs.
    #[test]
    fn tdr_duck_gate_defaults_to_device_duck_and_legacy_stays_selectable() {
        assert_eq!(DriverConfig::default().tdr_duck_mode, TDR_DUCK_DEVICE);
        assert_ne!(TDR_DUCK_DEVICE, TDR_DUCK_DISPLAY);
        // The registry read clamps out-of-range values to the default, so
        // the gate is a closed set of exactly these two.
        assert_eq!(TDR_DUCK_DEVICE, 0);
        assert_eq!(TDR_DUCK_DISPLAY, 1);
    }

    /// The gate has to survive the trip the driver actually takes it on:
    /// registry read → `DriverConfig` → `DeviceState::new` → the value the
    /// shell mirrors into its lock-free atomic at device add. Build 16
    /// stored the field and never read it back — the shell mirrored a
    /// separate local — so nothing anywhere proved the configured policy
    /// was the policy that ran. `shell::entry::device_add` now reads it
    /// through this accessor.
    #[test]
    fn tdr_duck_gate_survives_device_state_construction() {
        for mode in [TDR_DUCK_DEVICE, TDR_DUCK_DISPLAY] {
            let dev = DeviceState::new(
                DriverConfig { tdr_duck_mode: mode, ..DriverConfig::default() },
                None,
            );
            assert_eq!(dev.tdr_duck_mode(), mode);
        }
        // Restoring persisted state must not disturb the gate: the blob
        // carries identity reservations and the pool, never policy.
        let dev = DeviceState::new(
            DriverConfig { tdr_duck_mode: TDR_DUCK_DISPLAY, ..DriverConfig::default() },
            Some(&[0u8; 8]),
        );
        assert_eq!(dev.tdr_duck_mode(), TDR_DUCK_DISPLAY);
    }
}

// SPDX-License-Identifier: AGPL-3.0-only
//! The session model: SudoVDA's monitor lifecycle, upgraded with
//! libvirtualdisplay's identity/lease split (see THIRD-PARTY-NOTICES.md).
//!
//! One streaming client = one *lease* (`session_id`) on one virtual
//! monitor. The monitor's *identity* (`display_id` → EDID product code,
//! serial, connector) is separate and outlives the lease: a client that
//! reconnects with the same `display_id` gets the same connector and EDID
//! identity, so Windows recognizes the monitor and restores its settings.
//! Leases are fed by `PING` and reaped per-monitor (configurable 3 s–300 s,
//! or never for permanent-pool displays).
//!
//! Time is injected as milliseconds on a monotonic driver clock — this
//! module never reads a clock, which is what makes the watchdog testable.

use std::collections::BTreeMap;

use luminal_driver_proto::{
    create_flags, ring_state, CreateMonitorRequest, GetStatusReply, HandshakeReply,
    ModeSpec, MonitorStatus, ABI_MAX_MONITORS, DEFAULT_LEASE_TIMEOUT_MS,
    LEASE_TIMEOUT_DISABLED, LEASE_TIMEOUT_USE_DEFAULT, MAX_LEASE_TIMEOUT_MS,
    MIN_LEASE_TIMEOUT_MS, PROTO_VERSION_MAJOR, PROTO_VERSION_MINOR,
};

use crate::adapter::{select_adapter, AdapterInfo};
use crate::error::CoreError;
use crate::identity::{
    ephemeral_display_id, is_permanent_display_id, permanent_product_code,
    serial_from_display_id, temporary_product_code, ConnectorTable, EPHEMERAL_DISPLAY_ID_BASE,
};
use crate::modes::Mode;

/// A live virtual monitor.
#[derive(Clone, Debug)]
pub struct Monitor {
    /// Lease key.
    pub session_id: u64,
    /// Stable identity (possibly ephemeral-derived).
    pub display_id: u64,
    /// True when the identity was derived from the session (its connector
    /// reservation is dropped on destroy instead of retained).
    pub ephemeral_identity: bool,
    pub connector_index: u32,
    /// EDID identity, derived from `display_id` unless overridden.
    pub edid_serial: u32,
    pub product_code: u16,
    /// Validated mode list, preferred first. **The list actually IN
    /// FORCE** — what the monitor advertises and what every
    /// replug-from-`DeviceState` plugs with. `UPDATE_MODES` does not write
    /// it; only [`SessionTable::settle_modes`] does, and only after the OS
    /// push succeeded. See [`pending_modes`](Self::pending_modes).
    pub modes: Vec<Mode>,
    /// A merged list that has been handed to the shell for an
    /// `IddCxMonitorUpdateModes2` push but is NOT yet in force (build 17).
    ///
    /// This exists because the reply and the push are on different
    /// threads: `dispatch` completes the IRP, the effects worker calls the
    /// OS afterwards. Committing [`modes`](Self::modes) at dispatch time
    /// made the durable state claim modes the OS had not accepted — and
    /// then an identical retry merged to "nothing to add" and reported
    /// success while the monitor advertised nothing of the sort, so the
    /// caller could not recover. Keeping the merge here until
    /// [`settle_modes`](SessionTable::settle_modes) confirms the push is
    /// what makes a retry re-push.
    pub pending_modes: Option<PendingModes>,
    pub adapter_luid: u64,
    pub physical_width_mm: u32,
    pub physical_height_mm: u32,
    /// Desired HDR peak luminance in nits for the EDID's CTA-861.3 block.
    /// 0 => driver default (≈993 nits). Proto 0.4.
    pub max_nits: u32,
    pub friendly_name: [u16; 32],
    pub flags: u32,
    /// Effective lease timeout in ms; `u32::MAX` = never expires.
    pub lease_timeout_ms: u32,
    pub created_ms: u64,
    pub last_ping_ms: u64,
    /// Mirrors of ring telemetry, updated by the shell for `GET_STATUS`.
    pub ring_generation: u32,
    pub ring_state: u32,
    pub latest_sequence: u64,
    pub frames_published: u64,
    pub frames_dropped: u64,
    /// Last error recorded against this monitor (sticky until destroy).
    pub last_error: i32,
}

impl Monitor {
    pub fn preferred_mode(&self) -> &Mode {
        &self.modes[0]
    }

    /// The list a further `UPDATE_MODES` merges onto: the outstanding
    /// pending list if there is one, else the list in force. Merging onto
    /// the pending list keeps two requests in flight additive instead of
    /// letting the second silently discard the first's modes.
    fn merge_base(&self) -> &[Mode] {
        match &self.pending_modes {
            Some(p) => &p.modes,
            None => &self.modes,
        }
    }
}

/// A mode-list change that has been accepted by the driver and handed to
/// the shell, but is not in force until the OS push succeeds.
#[derive(Clone, Debug)]
pub struct PendingModes {
    /// Table-wide monotonic id of this update. Carried out to the shell
    /// with the effect and back in [`SessionTable::settle_modes`], so a
    /// settle that arrives after the session was destroyed and recreated,
    /// or after a newer update superseded this one, is a no-op instead of
    /// committing a list nobody asked for.
    pub seq: u64,
    /// The full post-merge list the shell is pushing.
    pub modes: Vec<Mode>,
}

/// What the shell's `IddCxMonitorUpdateModes2` push did, reported back to
/// the table by [`SessionTable::settle_modes`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeUpdateResult {
    /// The OS accepted the list; it is now what the monitor advertises.
    Applied,
    /// The push failed, or was deferred and never made (a TDR duck in
    /// flight, a torn-down adapter, a monitor that raced away). Either way
    /// the previous list is still in force, so the pending list is
    /// discarded and the next identical request merges — and pushes —
    /// again. The distinction between "refused" and "never attempted" is
    /// an ETW stage, not a state change here: both mean *not applied*.
    NotApplied,
}

/// Driver-side outcome of an `UPDATE_MODES` request, everything the reply
/// and the effect need.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModeUpdate {
    /// The full post-merge list. Already in force when `pending` is false;
    /// otherwise what the queued push carries.
    pub merged: Vec<Mode>,
    /// Requested entries the merged list advertises (already-present ones
    /// included).
    pub accepted: usize,
    /// Requested entries that did not fit under `MAX_MODES_PER_MONITOR`.
    pub dropped: usize,
    /// `Some(seq)` when THIS call created a pending update the caller must
    /// push. `None` when there was nothing new to add.
    pub queued: Option<u64>,
    /// True while any push for this monitor is outstanding — this call's
    /// or an earlier one's. `!pending` is the only state in which `merged`
    /// can be reported as actually in force.
    pub pending: bool,
}

/// The monitor table plus its policy knobs.
pub struct SessionTable {
    max_monitors: u32,
    watchdog_secs: u32,
    monitors: BTreeMap<u64, Monitor>,
    connectors: ConnectorTable,
    /// Monotonic id handed to each accepted `UPDATE_MODES`. Never reset —
    /// not even by `destroy` — so a settle for a torn-down session can
    /// never be mistaken for one belonging to its replacement.
    update_seq: u64,
}

/// Resolve the wire lease-timeout field against the driver default
/// (SudoVDA's global `watchdog_secs`, 0 = watchdog disabled).
pub fn effective_lease_timeout(requested_ms: u32, watchdog_secs: u32) -> u32 {
    match requested_ms {
        LEASE_TIMEOUT_USE_DEFAULT => {
            if watchdog_secs == 0 {
                LEASE_TIMEOUT_DISABLED
            } else {
                (watchdog_secs.saturating_mul(1000))
                    .clamp(MIN_LEASE_TIMEOUT_MS, MAX_LEASE_TIMEOUT_MS)
                    .max(DEFAULT_LEASE_TIMEOUT_MS.min(MAX_LEASE_TIMEOUT_MS))
                    .min(MAX_LEASE_TIMEOUT_MS)
            }
        }
        LEASE_TIMEOUT_DISABLED => LEASE_TIMEOUT_DISABLED,
        ms => ms.clamp(MIN_LEASE_TIMEOUT_MS, MAX_LEASE_TIMEOUT_MS),
    }
}

impl SessionTable {
    /// `max_monitors` and `watchdog_secs` come from registry config with
    /// SudoVDA-ported defaults. The cap is clamped to the ABI ceiling and
    /// doubles as the connector count.
    pub fn new(max_monitors: u32, watchdog_secs: u32) -> Self {
        let max_monitors = max_monitors.min(ABI_MAX_MONITORS);
        Self {
            max_monitors,
            watchdog_secs,
            monitors: BTreeMap::new(),
            connectors: ConnectorTable::new(max_monitors),
            update_seq: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.monitors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.monitors.is_empty()
    }

    pub fn get(&self, session_id: u64) -> Option<&Monitor> {
        self.monitors.get(&session_id)
    }

    pub fn get_mut(&mut self, session_id: u64) -> Option<&mut Monitor> {
        self.monitors.get_mut(&session_id)
    }

    pub fn monitors(&self) -> impl Iterator<Item = &Monitor> {
        self.monitors.values()
    }

    /// Restore connector reservations from the persistence blob at device
    /// start (identity retention across driver restarts).
    pub fn restore_reservations(&mut self, entries: impl IntoIterator<Item = (u64, u32)>) {
        self.connectors.restore(entries);
    }

    /// Current reservations, for the persistence blob.
    pub fn reservations(&self) -> Vec<(u64, u32)> {
        self.connectors.reservations().collect()
    }

    /// Build the handshake reply. Version compatibility is the caller's
    /// gate (`versions_compatible`); the reply always describes us
    /// truthfully so the host can log both sides on a refusal.
    pub fn handshake_reply(&self, drv_caps: u32, driver_build: u32) -> HandshakeReply {
        HandshakeReply {
            driver_proto_major: PROTO_VERSION_MAJOR,
            driver_proto_minor: PROTO_VERSION_MINOR,
            driver_build,
            caps: drv_caps,
            max_monitors: self.max_monitors,
            watchdog_secs: self.watchdog_secs,
        }
    }

    /// Validate and create a monitor. On success the returned reference
    /// carries the resolved identity, connector, adapter, and mode list;
    /// the shell then generates the EDID, plugs the IddCx monitor, and
    /// allocates the ring.
    ///
    /// `preferred_adapter` is the device-wide `SET_RENDER_ADAPTER` value
    /// (0 = none); it applies only when the request leaves `adapter_luid`
    /// unset, and silently falls back to largest-VRAM when stale.
    pub fn create(
        &mut self,
        now_ms: u64,
        req: &CreateMonitorRequest,
        drv_caps: u32,
        adapters: &[AdapterInfo],
        preferred_adapter: u64,
    ) -> Result<&Monitor, CoreError> {
        // Host ids may not squat on the reserved identity ranges (the
        // permanent pool goes through `create_trusted`).
        if req.display_id >= EPHEMERAL_DISPLAY_ID_BASE || is_permanent_display_id(req.display_id)
        {
            return Err(CoreError::IdentityInUse);
        }
        self.create_trusted(now_ms, req, drv_caps, adapters, preferred_adapter)
    }

    /// Create without the reserved-range gate — for driver-internal
    /// requests only (permanent pool members), never for host input.
    pub fn create_trusted(
        &mut self,
        now_ms: u64,
        req: &CreateMonitorRequest,
        drv_caps: u32,
        adapters: &[AdapterInfo],
        preferred_adapter: u64,
    ) -> Result<&Monitor, CoreError> {
        // session_id 0 is reserved as the wire value for "unset".
        if req.session_id == 0 {
            return Err(CoreError::BadMode);
        }
        if self.monitors.contains_key(&req.session_id) {
            return Err(CoreError::DuplicateSession);
        }
        if self.monitors.len() as u32 >= self.max_monitors {
            return Err(CoreError::MaxMonitors);
        }

        let ephemeral = req.display_id == 0
            || req.flags & create_flags::EPHEMERAL_IDENTITY != 0;
        let display_id = if ephemeral {
            ephemeral_display_id(req.session_id)
        } else {
            req.display_id
        };
        if self.monitors.values().any(|m| m.display_id == display_id) {
            return Err(CoreError::IdentityInUse);
        }

        let modes =
            Mode::validate_list(&req.modes, req.mode_count, req.bit_depth, req.hdr, drv_caps)?;

        let adapter_luid = if req.adapter_luid != 0 {
            select_adapter(adapters, req.adapter_luid)?
        } else if preferred_adapter != 0
            && select_adapter(adapters, preferred_adapter).is_ok()
        {
            preferred_adapter
        } else {
            select_adapter(adapters, 0)?
        };

        let active: Vec<u64> = self.monitors.values().map(|m| m.display_id).collect();
        let connector_index =
            self.connectors.acquire(display_id, &active).ok_or(CoreError::MaxMonitors)?;

        let product_code = if is_permanent_display_id(display_id) {
            permanent_product_code((display_id & 0xFFFF_FFFF) as u32)
        } else {
            temporary_product_code(connector_index)
        };
        let edid_serial = if req.edid_serial != 0 {
            req.edid_serial
        } else {
            serial_from_display_id(display_id)
        };

        let monitor = Monitor {
            session_id: req.session_id,
            display_id,
            ephemeral_identity: ephemeral,
            connector_index,
            edid_serial,
            product_code,
            modes,
            pending_modes: None,
            adapter_luid,
            physical_width_mm: req.physical_width_mm,
            physical_height_mm: req.physical_height_mm,
            max_nits: req.max_nits,
            friendly_name: req.friendly_name,
            flags: req.flags,
            lease_timeout_ms: effective_lease_timeout(req.lease_timeout_ms, self.watchdog_secs),
            created_ms: now_ms,
            // Creation counts as the first ping.
            last_ping_ms: now_ms,
            ring_generation: 1,
            ring_state: ring_state::ACTIVE,
            latest_sequence: 0,
            frames_published: 0,
            frames_dropped: 0,
            last_error: 0,
        };
        Ok(self.monitors.entry(req.session_id).or_insert(monitor))
    }

    /// `UPDATE_MODES` (proto 0.5): grow a live monitor's advertised mode
    /// list. Returns a [`ModeUpdate`] describing what the request bought.
    ///
    /// Bit depth and dynamic range are NOT taken from the request: they
    /// are monitor-wide, derived from an EDID that cannot be reissued on a
    /// live monitor, so every new entry is validated against the session's
    /// existing depth. A request that would need a different depth is
    /// simply a mode the envelope rejects — never a silent depth change.
    ///
    /// # The commit ordering, which is the point of this function
    ///
    /// This does NOT change [`Monitor::modes`]. The merged list is parked
    /// in [`Monitor::pending_modes`] and the caller pushes it at the OS;
    /// the list in force changes only when [`settle_modes`](Self::settle_modes)
    /// reports [`ModeUpdateResult::Applied`].
    ///
    /// Committing here instead — as build 17 originally did — is wrong in
    /// both directions: the IRP completes before the push runs, so on a
    /// failed or deferred push the durable list claimed modes the monitor
    /// did not advertise (and a replug from `DeviceState` would then plug
    /// them for real), and worse, the next IDENTICAL request merged to
    /// "nothing to add", produced no effect, and returned success — a
    /// permanent silent no-op with no way for the caller to recover.
    ///
    /// The pending list is still the host's INTENT and still lives here
    /// rather than only in the shell, because it is this table that every
    /// replug-from-`DeviceState` path (device re-add, D3Final
    /// re-bring-up, permanent-pool restore) plugs with — the commit just
    /// waits for proof.
    pub fn update_modes(
        &mut self,
        session_id: u64,
        specs: &[ModeSpec],
        drv_caps: u32,
    ) -> Result<ModeUpdate, CoreError> {
        let m = self.monitors.get(&session_id).ok_or(CoreError::NoSuchSession)?;
        // Monitor-wide depth/range, from the list the monitor already has.
        // Append-only merging never moves entry 0, so the pending list (if
        // any) has the same preferred mode and the same depth.
        let (bit_depth_raw, hdr_raw) = {
            let pref = m.preferred_mode();
            (pref.bit_depth.as_raw(), u32::from(pref.hdr))
        };
        let requested = Mode::validate_list(
            specs,
            specs.len() as u32,
            bit_depth_raw,
            hdr_raw,
            drv_caps,
        )?;
        let merge = Mode::merge_additive(m.merge_base(), &requested);
        if merge.appended == 0 {
            // Nothing new. Report whether an earlier push is still in
            // flight: with one outstanding, `merged` is a list the OS has
            // not accepted yet, and saying otherwise would be the same lie
            // in a different place.
            return Ok(ModeUpdate {
                merged: merge.merged,
                accepted: merge.accepted,
                dropped: merge.dropped,
                queued: None,
                pending: m.pending_modes.is_some(),
            });
        }
        // Only mutate after every fallible step has passed, so a rejected
        // request leaves the session exactly as it was.
        self.update_seq += 1;
        let seq = self.update_seq;
        self.monitors
            .get_mut(&session_id)
            .expect("looked up above")
            .pending_modes = Some(PendingModes { seq, modes: merge.merged.clone() });
        Ok(ModeUpdate {
            merged: merge.merged,
            accepted: merge.accepted,
            dropped: merge.dropped,
            queued: Some(seq),
            pending: true,
        })
    }

    /// Report what the shell's `IddCxMonitorUpdateModes2` push did with the
    /// pending update `seq` (build 17). Returns true when `seq` was still
    /// the monitor's outstanding update, i.e. when this settle decided
    /// anything.
    ///
    /// [`Applied`](ModeUpdateResult::Applied) is the ONLY thing that
    /// changes the list in force. [`NotApplied`](ModeUpdateResult::NotApplied)
    /// discards the pending list and records `err::UPDATE_FAILED` as the
    /// monitor's sticky last error, which is what makes the next identical
    /// request merge again, queue again, and genuinely re-push instead of
    /// collapsing into a success that means nothing.
    ///
    /// A stale `seq` — the session was destroyed and recreated, or a newer
    /// update superseded this one — settles nothing and is reported false.
    /// That is not a failure: the newer update carries a superset of this
    /// one's modes (merges are additive and stack on the pending list), so
    /// its own settle covers them.
    pub fn settle_modes(
        &mut self,
        session_id: u64,
        seq: u64,
        result: ModeUpdateResult,
    ) -> bool {
        let Some(m) = self.monitors.get_mut(&session_id) else {
            return false;
        };
        if m.pending_modes.as_ref().map(|p| p.seq) != Some(seq) {
            return false;
        }
        let pending = m.pending_modes.take().expect("matched above");
        match result {
            ModeUpdateResult::Applied => m.modes = pending.modes,
            ModeUpdateResult::NotApplied => {
                m.last_error = luminal_driver_proto::err::UPDATE_FAILED
            }
        }
        true
    }

    /// The list a pending update is trying to install, if any. Test and
    /// introspection helper — the shell reads the list out of the effect.
    pub fn pending_modes(&self, session_id: u64) -> Option<&[Mode]> {
        self.monitors.get(&session_id)?.pending_modes.as_ref().map(|p| p.modes.as_slice())
    }

    /// Explicit teardown at stream end. Retained identities keep their
    /// connector reservation (that's the point); ephemeral ones release it.
    pub fn destroy(&mut self, session_id: u64) -> Result<Monitor, CoreError> {
        let m = self.monitors.remove(&session_id).ok_or(CoreError::NoSuchSession)?;
        if m.ephemeral_identity {
            self.connectors.release(m.display_id);
        }
        Ok(m)
    }

    /// Remove every session — full runtime teardown (final device exit).
    /// Same identity semantics as [`destroy`](Self::destroy): ephemeral
    /// identities release their connector reservation, retained ones keep
    /// it. Needed because lease-disabled sessions (permanent-pool members)
    /// can never leave via [`tick`](Self::tick).
    pub fn destroy_all(&mut self) {
        let ids: Vec<u64> = self.monitors.keys().copied().collect();
        for id in ids {
            let _ = self.destroy(id);
        }
    }

    /// Feed the lease for one session.
    pub fn ping(&mut self, now_ms: u64, session_id: u64) -> Result<(), CoreError> {
        let m = self.monitors.get_mut(&session_id).ok_or(CoreError::NoSuchSession)?;
        m.last_ping_ms = now_ms;
        Ok(())
    }

    /// Lease introspection: (display_id, connector, remaining ms).
    pub fn query_lease(
        &self,
        now_ms: u64,
        session_id: u64,
    ) -> Result<(u64, u32, u32), CoreError> {
        let m = self.monitors.get(&session_id).ok_or(CoreError::NoSuchSession)?;
        let remaining = if m.lease_timeout_ms == LEASE_TIMEOUT_DISABLED {
            u32::MAX
        } else {
            let elapsed = now_ms.saturating_sub(m.last_ping_ms);
            u64::from(m.lease_timeout_ms).saturating_sub(elapsed).min(u64::from(u32::MAX)) as u32
        };
        Ok((m.display_id, m.connector_index, remaining))
    }

    /// Watchdog sweep: remove and return every monitor whose lease
    /// expired. Call from the shell's 1 s timer.
    pub fn tick(&mut self, now_ms: u64) -> Vec<Monitor> {
        let dead: Vec<u64> = self
            .monitors
            .values()
            .filter(|m| {
                m.lease_timeout_ms != LEASE_TIMEOUT_DISABLED
                    && now_ms.saturating_sub(m.last_ping_ms) > u64::from(m.lease_timeout_ms)
            })
            .map(|m| m.session_id)
            .collect();
        dead.into_iter()
            .map(|id| {
                let m = self.monitors.remove(&id).expect("id came from the table");
                if m.ephemeral_identity {
                    self.connectors.release(m.display_id);
                }
                m
            })
            .collect()
    }

    /// Fill the `GET_STATUS` reply.
    pub fn status(&self, uptime_ms: u64, driver_build: u32, drv_caps: u32) -> GetStatusReply {
        let mut reply = GetStatusReply {
            uptime_ms,
            driver_build,
            proto_major: PROTO_VERSION_MAJOR,
            proto_minor: PROTO_VERSION_MINOR,
            caps: drv_caps,
            max_monitors: self.max_monitors,
            watchdog_secs: self.watchdog_secs,
            monitor_count: 0,
            monitors: [zero_status(); ABI_MAX_MONITORS as usize],
        };
        for (i, m) in self.monitors.values().take(ABI_MAX_MONITORS as usize).enumerate() {
            let pref = m.preferred_mode();
            reply.monitors[i] = MonitorStatus {
                session_id: m.session_id,
                display_id: m.display_id,
                adapter_luid: m.adapter_luid,
                latest_sequence: m.latest_sequence,
                frames_published: m.frames_published,
                frames_dropped: m.frames_dropped,
                last_ping_ms: m.last_ping_ms,
                width: pref.width,
                height: pref.height,
                refresh_millihz: pref.refresh_millihz,
                bit_depth: pref.bit_depth.as_raw(),
                hdr: u32::from(pref.hdr),
                ring_generation: m.ring_generation,
                ring_state: m.ring_state,
                last_error: m.last_error,
                connector_index: m.connector_index,
                lease_timeout_ms: m.lease_timeout_ms,
            };
            reply.monitor_count = (i + 1) as u32;
        }
        reply
    }
}

const fn zero_status() -> MonitorStatus {
    MonitorStatus {
        session_id: 0,
        display_id: 0,
        adapter_luid: 0,
        latest_sequence: 0,
        frames_published: 0,
        frames_dropped: 0,
        last_ping_ms: 0,
        width: 0,
        height: 0,
        refresh_millihz: 0,
        bit_depth: 0,
        hdr: 0,
        ring_generation: 0,
        ring_state: 0,
        last_error: 0,
        connector_index: 0,
        lease_timeout_ms: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use luminal_driver_proto::{caps, ModeSpec, DEFAULT_MAX_MONITORS, DEFAULT_WATCHDOG_SECS};

    const CAPS: u32 = caps::HDR10 | caps::SDR10_BIT | caps::MULTI_MODE;

    fn adapters() -> Vec<AdapterInfo> {
        vec![
            AdapterInfo { luid: 0x10, vram_bytes: 8 << 30, name: "iGPU".into(), software: false },
            AdapterInfo { luid: 0x20, vram_bytes: 16 << 30, name: "dGPU".into(), software: false },
        ]
    }

    fn req(session_id: u64) -> CreateMonitorRequest {
        let mut modes = [ModeSpec::default(); 4];
        modes[0] = ModeSpec { width: 1920, height: 1080, refresh_millihz: 60_000 };
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

    fn stable_req(session_id: u64, display_id: u64) -> CreateMonitorRequest {
        let mut r = req(session_id);
        r.display_id = display_id;
        r
    }

    fn table() -> SessionTable {
        SessionTable::new(DEFAULT_MAX_MONITORS, DEFAULT_WATCHDOG_SECS)
    }

    #[test]
    fn create_ping_destroy_lifecycle() {
        let mut t = table();
        let m = t.create(1000, &req(7), CAPS, &adapters(), 0).unwrap();
        assert_eq!(m.adapter_luid, 0x20, "default = largest VRAM");
        assert_eq!(m.ring_generation, 1);
        assert!(m.ephemeral_identity, "display_id 0 derives ephemeral");
        assert_eq!(m.connector_index, 0);
        assert_ne!(m.edid_serial, 0, "serial derived from identity");
        assert_eq!(t.len(), 1);

        t.ping(2000, 7).unwrap();
        assert_eq!(t.get(7).unwrap().last_ping_ms, 2000);

        let gone = t.destroy(7).unwrap();
        assert_eq!(gone.session_id, 7);
        assert!(t.is_empty());
        assert_eq!(t.destroy(7).err(), Some(CoreError::NoSuchSession));
    }

    #[test]
    fn stable_identity_reclaims_connector_and_serial_across_leases() {
        let mut t = table();
        let m1 = t.create(0, &stable_req(1, 0xCAFE), CAPS, &adapters(), 0).unwrap();
        let (conn, serial, product) = (m1.connector_index, m1.edid_serial, m1.product_code);
        assert!(!m1.ephemeral_identity);

        // Other clients take connectors while CAFE is away.
        t.destroy(1).unwrap();
        t.create(0, &req(2), CAPS, &adapters(), 0).unwrap();

        // CAFE returns under a NEW lease: same connector, same identity.
        let m2 = t.create(0, &stable_req(3, 0xCAFE), CAPS, &adapters(), 0).unwrap();
        assert_eq!(m2.connector_index, conn);
        assert_eq!(m2.edid_serial, serial);
        assert_eq!(m2.product_code, product);
    }

    #[test]
    fn ephemeral_identity_frees_connector_and_live_identity_is_exclusive() {
        let mut t = SessionTable::new(3, 3);
        t.create(0, &req(1), CAPS, &adapters(), 0).unwrap();
        // Same stable identity twice concurrently: refused.
        t.create(0, &stable_req(2, 0xAA), CAPS, &adapters(), 0).unwrap();
        assert_eq!(
            t.create(0, &stable_req(3, 0xAA), CAPS, &adapters(), 0).err(),
            Some(CoreError::IdentityInUse)
        );
        // Reserved identity ranges are refused outright.
        let mut t2 = table();
        assert_eq!(
            t2.create(0, &stable_req(1, EPHEMERAL_DISPLAY_ID_BASE | 5), CAPS, &adapters(), 0)
                .err(),
            Some(CoreError::IdentityInUse)
        );
    }

    #[test]
    fn preferred_adapter_applies_only_when_request_unset_and_falls_back_when_stale() {
        let mut t = table();
        // Preference honored.
        let m = t.create(0, &req(1), CAPS, &adapters(), 0x10).unwrap();
        assert_eq!(m.adapter_luid, 0x10);
        // Explicit request wins over preference.
        let mut r = req(2);
        r.adapter_luid = 0x20;
        assert_eq!(t.create(0, &r, CAPS, &adapters(), 0x10).unwrap().adapter_luid, 0x20);
        // Stale preference falls back to largest-VRAM instead of failing.
        assert_eq!(t.create(0, &req(3), CAPS, &adapters(), 0xDEAD).unwrap().adapter_luid, 0x20);
    }

    #[test]
    fn lease_timeouts_resolve_and_clamp() {
        assert_eq!(effective_lease_timeout(0, 3), 10_000, "default floor");
        // The advertised default must be the enforced one: a handshake
        // reporting DEFAULT_WATCHDOG_SECS and a USE_DEFAULT lease must
        // agree (the 3 s / 10 s mismatch shipped through alpha.3).
        assert_eq!(
            effective_lease_timeout(0, DEFAULT_WATCHDOG_SECS),
            DEFAULT_WATCHDOG_SECS * 1000,
            "advertised watchdog matches the enforced lease floor"
        );
        assert_eq!(effective_lease_timeout(0, 60), 60_000);
        assert_eq!(effective_lease_timeout(0, 0), LEASE_TIMEOUT_DISABLED);
        assert_eq!(effective_lease_timeout(LEASE_TIMEOUT_DISABLED, 3), LEASE_TIMEOUT_DISABLED);
        assert_eq!(effective_lease_timeout(1, 3), MIN_LEASE_TIMEOUT_MS);
        assert_eq!(effective_lease_timeout(9_999_999, 3), MAX_LEASE_TIMEOUT_MS);
        assert_eq!(effective_lease_timeout(15_000, 3), 15_000);
    }

    #[test]
    fn per_lease_watchdog_reaps_independently() {
        let mut t = table();
        let mut short = req(1);
        short.lease_timeout_ms = 3_000;
        let mut long = req(2);
        long.lease_timeout_ms = 30_000;
        let mut never = req(3);
        never.lease_timeout_ms = LEASE_TIMEOUT_DISABLED;
        t.create(0, &short, CAPS, &adapters(), 0).unwrap();
        t.create(0, &long, CAPS, &adapters(), 0).unwrap();
        t.create(0, &never, CAPS, &adapters(), 0).unwrap();

        assert!(t.tick(3_000).is_empty(), "at deadline: not yet");
        let reaped = t.tick(3_001);
        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0].session_id, 1);

        let reaped = t.tick(30_001);
        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0].session_id, 2);

        assert!(t.tick(u64::MAX).is_empty(), "disabled lease never reaps");
    }

    #[test]
    fn query_lease_reports_remaining() {
        let mut t = table();
        let mut r = req(1);
        r.lease_timeout_ms = 10_000;
        t.create(0, &r, CAPS, &adapters(), 0).unwrap();
        t.ping(5_000, 1).unwrap();
        let (_, _, remaining) = t.query_lease(9_000, 1).unwrap();
        assert_eq!(remaining, 6_000);
        let (_, _, remaining) = t.query_lease(99_000, 1).unwrap();
        assert_eq!(remaining, 0, "expired but not yet ticked");
        assert_eq!(t.query_lease(0, 9).err(), Some(CoreError::NoSuchSession));
    }

    #[test]
    fn multi_mode_lists_are_stored_preferred_first() {
        let mut t = table();
        let mut r = req(1);
        r.mode_count = 2;
        r.modes[1] = ModeSpec { width: 1920, height: 1080, refresh_millihz: 120_000 };
        let m = t.create(0, &r, CAPS, &adapters(), 0).unwrap();
        assert_eq!(m.modes.len(), 2);
        assert_eq!(m.preferred_mode().refresh_millihz, 60_000);

        // Bad list refused before touching the table.
        let mut bad = req(2);
        bad.mode_count = 0;
        assert_eq!(t.create(0, &bad, CAPS, &adapters(), 0).err(), Some(CoreError::BadMode));
        assert_eq!(t.len(), 1);
    }

    /// Build 17: the durable half of a live mode-list change. This copy —
    /// not the shell's runtime one — is what every replug-from-DeviceState
    /// path plugs with, so if the update did not land here it would revert
    /// on the next device re-add, D3Final re-bring-up, or pool restore.
    /// It lands only once the OS push is confirmed.
    #[test]
    fn update_modes_appends_to_the_stored_list_without_disturbing_the_preferred_mode() {
        let mut t = table();
        t.create(0, &req(1), CAPS, &adapters(), 0).unwrap();
        assert_eq!(t.get(1).unwrap().modes.len(), 1);

        // The motivating case: add the frame-generation-doubled rate to a
        // monitor created at the base rate.
        let doubled = ModeSpec { width: 1920, height: 1080, refresh_millihz: 120_000 };
        let u = t.update_modes(1, &[doubled], CAPS).unwrap();
        assert_eq!((u.merged.len(), u.accepted, u.dropped), (2, 1, 0));
        let seq = u.queued.expect("a push is required");
        assert!(u.pending, "not in force until the OS says so");
        assert_eq!(t.get(1).unwrap().modes.len(), 1, "list in force unchanged so far");
        assert_eq!(t.pending_modes(1).unwrap().len(), 2);

        // The push succeeded: NOW it is durable.
        assert!(t.settle_modes(1, seq, ModeUpdateResult::Applied));
        let m = t.get(1).unwrap();
        assert_eq!(m.modes.len(), 2);
        assert_eq!(m.preferred_mode().refresh_millihz, 60_000, "preferred untouched");
        assert_eq!(m.modes[1].refresh_millihz, 120_000);
        assert!(m.pending_modes.is_none());
        assert_eq!(m.last_error, 0);

        // Idempotent: resending a list that is genuinely in force is a
        // no-op, not a second push at the OS.
        let u = t.update_modes(1, &[doubled], CAPS).unwrap();
        assert_eq!((u.merged.len(), u.queued, u.pending), (2, None, false));
    }

    /// FINDINGS 1/3/4, the durable half. A push that did not take must
    /// leave the table exactly where it was, and the next identical
    /// request must produce a NEW push — the old code committed at request
    /// time, so the retry merged to nothing and reported success forever.
    #[test]
    fn a_push_that_did_not_take_leaves_the_list_in_force_alone_and_stays_retryable() {
        let mut t = table();
        t.create(0, &req(1), CAPS, &adapters(), 0).unwrap();
        let before = t.get(1).unwrap().modes.clone();
        let doubled = ModeSpec { width: 1920, height: 1080, refresh_millihz: 120_000 };

        // Refused by the OS and never attempted (deferred) are the SAME
        // state change here — the ETW stage is what tells them apart —
        // which is exactly why a deferral is as retryable as a failure.
        let first = t.update_modes(1, &[doubled], CAPS).unwrap();
        let seq = first.queued.expect("first request queues a push");
        assert!(t.settle_modes(1, seq, ModeUpdateResult::NotApplied));
        assert_eq!(t.get(1).unwrap().modes, before, "modes unchanged, carry on");
        assert!(t.pending_modes(1).is_none(), "pending discarded");
        assert_eq!(
            t.get(1).unwrap().last_error,
            luminal_driver_proto::err::UPDATE_FAILED,
            "surfaced in GET_STATUS"
        );

        // The retry: a REAL second push, with a fresh id.
        let retry = t.update_modes(1, &[doubled], CAPS).unwrap();
        let retry_seq = retry.queued.expect("the retry must actually re-push");
        assert_ne!(retry_seq, seq);
        assert!(retry.pending);
        assert_eq!(t.get(1).unwrap().modes, before, "still not in force");

        // ...and when that one lands, it lands.
        assert!(t.settle_modes(1, retry_seq, ModeUpdateResult::Applied));
        assert_eq!(t.get(1).unwrap().modes.len(), 2);
    }

    /// A settle can arrive late — after a newer update superseded it, or
    /// after the session was destroyed and its id reused. Neither may
    /// commit a list nobody is pushing.
    #[test]
    fn a_stale_settle_commits_nothing() {
        let mut t = table();
        t.create(0, &req(1), CAPS, &adapters(), 0).unwrap();
        let a = ModeSpec { width: 1920, height: 1080, refresh_millihz: 120_000 };
        let b = ModeSpec { width: 1280, height: 720, refresh_millihz: 60_000 };

        let first = t.update_modes(1, &[a], CAPS).unwrap().queued.unwrap();
        // A second request stacks onto the pending list (additive), so the
        // first push's list is a subset of the second's.
        let second = t.update_modes(1, &[b], CAPS).unwrap();
        let second_seq = second.queued.unwrap();
        assert_eq!(second.merged.len(), 3, "merged onto the pending list, not the live one");

        assert!(!t.settle_modes(1, first, ModeUpdateResult::Applied), "superseded");
        assert_eq!(t.get(1).unwrap().modes.len(), 1);
        assert!(t.settle_modes(1, second_seq, ModeUpdateResult::Applied));
        assert_eq!(t.get(1).unwrap().modes.len(), 3);

        // Destroy + recreate the same session id, then settle the dead
        // update: the ids are table-wide and never reused, so it misses.
        let third = t.update_modes(1, &[ModeSpec { width: 640, height: 480, refresh_millihz: 60_000 }], CAPS)
            .unwrap()
            .queued
            .unwrap();
        t.destroy(1).unwrap();
        t.create(0, &req(1), CAPS, &adapters(), 0).unwrap();
        assert!(!t.settle_modes(1, third, ModeUpdateResult::Applied));
        assert_eq!(t.get(1).unwrap().modes.len(), 1, "the replacement keeps its own list");
    }

    /// FINDING 2, durable half: at the cap the merge keeps what fits and
    /// counts what it could not, so the caller can report partial
    /// application instead of plain success.
    #[test]
    fn update_modes_reports_what_did_not_fit() {
        let mut t = table();
        let mut r = req(1);
        r.mode_count = 3;
        r.modes[1] = ModeSpec { width: 1920, height: 1080, refresh_millihz: 90_000 };
        r.modes[2] = ModeSpec { width: 1920, height: 1080, refresh_millihz: 120_000 };
        t.create(0, &r, CAPS, &adapters(), 0).unwrap();

        let u = t
            .update_modes(
                1,
                &[
                    ModeSpec { width: 1920, height: 1080, refresh_millihz: 144_000 },
                    ModeSpec { width: 1920, height: 1080, refresh_millihz: 240_000 },
                ],
                CAPS,
            )
            .unwrap();
        assert_eq!((u.accepted, u.dropped), (1, 1), "one fit, one did not");
        assert_eq!(u.merged.len(), 4);
        let seq = u.queued.unwrap();
        assert!(t.settle_modes(1, seq, ModeUpdateResult::Applied));
        assert_eq!(t.get(1).unwrap().modes.len(), 4);

        // The list is full now: a further request is all-dropped, and that
        // is still not an error — nothing was lost, nothing was gained.
        let u = t
            .update_modes(1, &[ModeSpec { width: 1920, height: 1080, refresh_millihz: 240_000 }], CAPS)
            .unwrap();
        assert_eq!((u.accepted, u.dropped, u.queued, u.pending), (0, 1, None, false));
    }

    #[test]
    fn update_modes_rejects_bad_input_without_touching_the_session() {
        let mut t = table();
        t.create(0, &req(1), CAPS, &adapters(), 0).unwrap();
        let before = t.get(1).unwrap().modes.clone();

        // Unknown session.
        assert_eq!(
            t.update_modes(99, &[ModeSpec { width: 1920, height: 1080, refresh_millihz: 120_000 }], CAPS)
                .err(),
            Some(CoreError::NoSuchSession)
        );
        // Empty list.
        assert_eq!(t.update_modes(1, &[], CAPS).err(), Some(CoreError::BadMode));
        // Out-of-envelope entry: the whole request fails, nothing partial.
        let bad = [
            ModeSpec { width: 1920, height: 1080, refresh_millihz: 120_000 },
            ModeSpec { width: 3, height: 3, refresh_millihz: 1 },
        ];
        assert_eq!(t.update_modes(1, &bad, CAPS).err(), Some(CoreError::BadMode));
        // Over the cap.
        let many = [ModeSpec { width: 1920, height: 1080, refresh_millihz: 30_000 }; 5];
        assert_eq!(t.update_modes(1, &many, CAPS).err(), Some(CoreError::BadMode));
        assert_eq!(t.get(1).unwrap().modes, before, "session untouched by every rejection");
        assert!(t.pending_modes(1).is_none(), "and nothing queued at the OS");
    }

    /// The monitor-wide depth is EDID-derived and the EDID cannot be
    /// reissued on a live monitor, so an update can never change it: new
    /// entries are validated against the session's existing depth, and a
    /// caps-gated depth the driver lacks cannot sneak in through this path.
    #[test]
    fn update_modes_inherits_the_sessions_bit_depth() {
        let mut t = table();
        let mut hdr = req(1);
        hdr.bit_depth = 110;
        hdr.hdr = 1;
        t.create(0, &hdr, CAPS, &adapters(), 0).unwrap();
        let u = t
            .update_modes(1, &[ModeSpec { width: 1920, height: 1080, refresh_millihz: 120_000 }], CAPS)
            .unwrap();
        assert_eq!(u.accepted, 1);
        assert!(u.queued.is_some());
        assert!(u.merged.iter().all(|m| m.hdr && m.bit_depth.as_raw() == 110));

        // Same monitor, a driver WITHOUT the HDR cap: the existing depth no
        // longer validates, so the update fails cleanly instead of
        // downgrading the monitor's depth behind the host's back.
        assert_eq!(
            t.update_modes(1, &[ModeSpec { width: 1280, height: 720, refresh_millihz: 60_000 }], caps::MULTI_MODE)
                .err(),
            Some(CoreError::HdrUnsupported)
        );
    }

    #[test]
    fn max_monitors_cap_enforced_and_abi_clamped() {
        let mut t = SessionTable::new(2, 3);
        t.create(0, &req(1), CAPS, &adapters(), 0).unwrap();
        t.create(0, &req(2), CAPS, &adapters(), 0).unwrap();
        assert_eq!(t.create(0, &req(3), CAPS, &adapters(), 0).err(), Some(CoreError::MaxMonitors));
        t.destroy(1).unwrap();
        assert!(t.create(0, &req(3), CAPS, &adapters(), 0).is_ok());

        let t = SessionTable::new(999, 3);
        assert_eq!(t.handshake_reply(CAPS, 1).max_monitors, ABI_MAX_MONITORS);
    }

    #[test]
    fn reservations_survive_restore_round_trip() {
        let mut t = table();
        t.create(0, &stable_req(1, 0xAB), CAPS, &adapters(), 0).unwrap();
        let saved = t.reservations();
        assert_eq!(saved, vec![(0xAB, 0)]);

        // Fresh table (driver restart) restores the reservation.
        let mut t2 = table();
        t2.restore_reservations(saved);
        // Another identity arrives first but does NOT take connector 0.
        let other = t2.create(0, &stable_req(5, 0xCD), CAPS, &adapters(), 0).unwrap();
        assert_eq!(other.connector_index, 1);
        let back = t2.create(0, &stable_req(6, 0xAB), CAPS, &adapters(), 0).unwrap();
        assert_eq!(back.connector_index, 0);
    }

    #[test]
    fn status_reflects_identity_and_lease_fields() {
        let mut t = table();
        t.create(100, &stable_req(1, 0xAB), CAPS, &adapters(), 0).unwrap();
        {
            let m = t.get_mut(1).unwrap();
            m.frames_published = 500;
            m.ring_generation = 3;
        }
        let s = t.status(9999, 77, CAPS);
        assert_eq!(s.monitor_count, 1);
        let m1 = s.monitors[0];
        assert_eq!(m1.display_id, 0xAB);
        assert_eq!(m1.connector_index, 0);
        assert_eq!(m1.lease_timeout_ms, 10_000);
        assert_eq!(m1.frames_published, 500);
        assert_eq!(m1.ring_generation, 3);
        assert_eq!(s.monitors[1].session_id, 0);
    }
}

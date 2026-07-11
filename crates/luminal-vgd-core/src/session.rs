// SPDX-License-Identifier: AGPL-3.0-only
//! The session model — SudoVDA's core behavior, ported.
//!
//! One streaming client = one `session_id` = one virtual monitor with an
//! exact single-entry mode list. Monitors are created and destroyed by
//! IOCTL, capped globally, and reaped by a PING-fed watchdog so a crashed
//! host never leaves zombie displays (SudoVDA: default 3 s, 0 disables).
//!
//! Time is injected as milliseconds on a monotonic driver clock — this
//! module never reads a clock, which is what makes the watchdog testable.

use std::collections::BTreeMap;

use luminal_driver_proto::{
    ring_state, CreateMonitorRequest, GetStatusReply, HandshakeReply, MonitorStatus,
    ABI_MAX_MONITORS, PROTO_VERSION_MAJOR, PROTO_VERSION_MINOR,
};

use crate::adapter::{select_adapter, AdapterInfo};
use crate::error::CoreError;
use crate::modes::Mode;

/// A live virtual monitor.
#[derive(Clone, Debug)]
pub struct Monitor {
    pub session_id: u64,
    pub mode: Mode,
    pub adapter_luid: u64,
    pub edid_serial: u32,
    pub friendly_name: [u16; 32],
    pub flags: u32,
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

/// The monitor table plus its policy knobs.
pub struct SessionTable {
    max_monitors: u32,
    watchdog_secs: u32,
    monitors: BTreeMap<u64, Monitor>,
}

impl SessionTable {
    /// `max_monitors` and `watchdog_secs` come from registry config with
    /// SudoVDA-ported defaults (`DEFAULT_MAX_MONITORS`,
    /// `DEFAULT_WATCHDOG_SECS`). The cap is clamped to the ABI ceiling.
    pub fn new(max_monitors: u32, watchdog_secs: u32) -> Self {
        Self {
            max_monitors: max_monitors.min(ABI_MAX_MONITORS),
            watchdog_secs,
            monitors: BTreeMap::new(),
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
    /// carries the resolved adapter and validated mode; the shell then
    /// generates the EDID, plugs the IddCx monitor, and allocates the ring.
    pub fn create(
        &mut self,
        now_ms: u64,
        req: &CreateMonitorRequest,
        drv_caps: u32,
        adapters: &[AdapterInfo],
    ) -> Result<&Monitor, CoreError> {
        // session_id 0 is reserved as the wire value for "unset".
        if req.session_id == 0 {
            return Err(CoreError::BadMode);
        }
        if self.monitors.contains_key(&req.session_id) {
            // SudoVDA had no duplicate-create story (Apollo never reused
            // ids); we make it explicit: destroy first, then recreate.
            return Err(CoreError::DuplicateSession);
        }
        if self.monitors.len() as u32 >= self.max_monitors {
            return Err(CoreError::MaxMonitors);
        }
        let mode = Mode::validate(
            req.width,
            req.height,
            req.refresh_millihz,
            req.bit_depth,
            req.hdr,
            drv_caps,
        )?;
        let adapter_luid = select_adapter(adapters, req.adapter_luid)?;

        let monitor = Monitor {
            session_id: req.session_id,
            mode,
            adapter_luid,
            edid_serial: req.edid_serial,
            friendly_name: req.friendly_name,
            flags: req.flags,
            created_ms: now_ms,
            // Creation counts as the first ping: a host that creates and
            // immediately crashes still gets reaped one watchdog later.
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

    /// Explicit teardown at stream end. Returns the record so the shell
    /// can unplug the IddCx monitor and free the ring.
    pub fn destroy(&mut self, session_id: u64) -> Result<Monitor, CoreError> {
        self.monitors.remove(&session_id).ok_or(CoreError::NoSuchSession)
    }

    /// Feed the watchdog for one session.
    pub fn ping(&mut self, now_ms: u64, session_id: u64) -> Result<(), CoreError> {
        let m = self.monitors.get_mut(&session_id).ok_or(CoreError::NoSuchSession)?;
        m.last_ping_ms = now_ms;
        Ok(())
    }

    /// Watchdog sweep: remove and return every monitor whose owner has
    /// been silent longer than the timeout. Call periodically (the shell
    /// runs this on a timer; period ≤ 1 s keeps reap latency near the
    /// configured value). `watchdog_secs == 0` disables reaping entirely.
    pub fn tick(&mut self, now_ms: u64) -> Vec<Monitor> {
        if self.watchdog_secs == 0 {
            return Vec::new();
        }
        let deadline = u64::from(self.watchdog_secs) * 1000;
        let dead: Vec<u64> = self
            .monitors
            .values()
            .filter(|m| now_ms.saturating_sub(m.last_ping_ms) > deadline)
            .map(|m| m.session_id)
            .collect();
        dead.into_iter()
            .map(|id| self.monitors.remove(&id).expect("id came from the table"))
            .collect()
    }

    /// Fill the `GET_STATUS` reply.
    pub fn status(
        &self,
        uptime_ms: u64,
        driver_build: u32,
        drv_caps: u32,
    ) -> GetStatusReply {
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
            reply.monitors[i] = MonitorStatus {
                session_id: m.session_id,
                adapter_luid: m.adapter_luid,
                latest_sequence: m.latest_sequence,
                frames_published: m.frames_published,
                frames_dropped: m.frames_dropped,
                last_ping_ms: m.last_ping_ms,
                width: m.mode.width,
                height: m.mode.height,
                refresh_millihz: m.mode.refresh_millihz,
                bit_depth: m.mode.bit_depth.as_raw(),
                hdr: u32::from(m.mode.hdr),
                ring_generation: m.ring_generation,
                ring_state: m.ring_state,
                last_error: m.last_error,
            };
            reply.monitor_count = (i + 1) as u32;
        }
        reply
    }
}

const fn zero_status() -> MonitorStatus {
    MonitorStatus {
        session_id: 0,
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use luminal_driver_proto::{caps, DEFAULT_MAX_MONITORS, DEFAULT_WATCHDOG_SECS};

    const CAPS: u32 = caps::HDR10 | caps::SDR10_BIT;

    fn adapters() -> Vec<AdapterInfo> {
        vec![
            AdapterInfo { luid: 0x10, vram_bytes: 8 << 30, name: "iGPU".into(), software: false },
            AdapterInfo { luid: 0x20, vram_bytes: 16 << 30, name: "dGPU".into(), software: false },
        ]
    }

    fn req(session_id: u64) -> CreateMonitorRequest {
        CreateMonitorRequest {
            session_id,
            adapter_luid: 0,
            width: 1920,
            height: 1080,
            refresh_millihz: 60_000,
            bit_depth: 8,
            hdr: 0,
            edid_serial: session_id as u32,
            flags: 0,
            reserved: 0,
            friendly_name: [0; 32],
        }
    }

    fn table() -> SessionTable {
        SessionTable::new(DEFAULT_MAX_MONITORS, DEFAULT_WATCHDOG_SECS)
    }

    #[test]
    fn create_ping_destroy_lifecycle() {
        let mut t = table();
        let m = t.create(1000, &req(7), CAPS, &adapters()).unwrap();
        assert_eq!(m.adapter_luid, 0x20, "default = largest VRAM");
        assert_eq!(m.ring_generation, 1);
        assert_eq!(t.len(), 1);

        t.ping(2000, 7).unwrap();
        assert_eq!(t.get(7).unwrap().last_ping_ms, 2000);

        let gone = t.destroy(7).unwrap();
        assert_eq!(gone.session_id, 7);
        assert!(t.is_empty());
        assert_eq!(t.destroy(7).err(), Some(CoreError::NoSuchSession));
        assert_eq!(t.ping(0, 7), Err(CoreError::NoSuchSession));
    }

    #[test]
    fn duplicate_session_refused() {
        let mut t = table();
        t.create(0, &req(7), CAPS, &adapters()).unwrap();
        assert_eq!(
            t.create(1, &req(7), CAPS, &adapters()).err(),
            Some(CoreError::DuplicateSession)
        );
    }

    #[test]
    fn session_id_zero_reserved() {
        let mut t = table();
        assert!(t.create(0, &req(0), CAPS, &adapters()).is_err());
    }

    #[test]
    fn max_monitors_cap_enforced_and_abi_clamped() {
        let mut t = SessionTable::new(2, 3);
        t.create(0, &req(1), CAPS, &adapters()).unwrap();
        t.create(0, &req(2), CAPS, &adapters()).unwrap();
        assert_eq!(t.create(0, &req(3), CAPS, &adapters()).err(), Some(CoreError::MaxMonitors));

        // Destroy frees a slot.
        t.destroy(1).unwrap();
        assert!(t.create(0, &req(3), CAPS, &adapters()).is_ok());

        // A registry value above the ABI ceiling is clamped.
        let t = SessionTable::new(999, 3);
        assert_eq!(t.handshake_reply(CAPS, 1).max_monitors, ABI_MAX_MONITORS);
    }

    #[test]
    fn watchdog_reaps_silent_sessions_only() {
        let mut t = table(); // 3 s watchdog
        t.create(0, &req(1), CAPS, &adapters()).unwrap();
        t.create(0, &req(2), CAPS, &adapters()).unwrap();

        // Session 1 keeps pinging; session 2 goes silent.
        t.ping(2900, 1).unwrap();
        assert!(t.tick(3000).is_empty(), "nobody past deadline yet");

        t.ping(5000, 1).unwrap();
        let reaped = t.tick(3001 /* 2's create-time ping + 3s + 1ms */);
        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0].session_id, 2);
        assert!(t.get(1).is_some());
        assert!(t.get(2).is_none());
    }

    #[test]
    fn watchdog_counts_from_creation() {
        // Create-then-crash host: never pinged, still reaped.
        let mut t = table();
        t.create(1000, &req(1), CAPS, &adapters()).unwrap();
        assert!(t.tick(4000).is_empty(), "exactly at deadline: not yet");
        assert_eq!(t.tick(4001).len(), 1);
    }

    #[test]
    fn watchdog_zero_disables() {
        let mut t = SessionTable::new(10, 0);
        t.create(0, &req(1), CAPS, &adapters()).unwrap();
        assert!(t.tick(u64::MAX).is_empty());
    }

    #[test]
    fn create_rejects_bad_mode_before_touching_table() {
        let mut t = table();
        let mut r = req(1);
        r.width = 100; // under envelope
        assert_eq!(t.create(0, &r, CAPS, &adapters()).err(), Some(CoreError::BadMode));
        assert!(t.is_empty());

        let mut r = req(1);
        r.adapter_luid = 0xDEAD; // unknown adapter
        assert_eq!(t.create(0, &r, CAPS, &adapters()).err(), Some(CoreError::NoAdapter));
        assert!(t.is_empty());
    }

    #[test]
    fn status_reflects_table_and_telemetry() {
        let mut t = table();
        t.create(100, &req(1), CAPS, &adapters()).unwrap();
        t.create(200, &req(2), CAPS, &adapters()).unwrap();
        {
            let m = t.get_mut(1).unwrap();
            m.frames_published = 500;
            m.frames_dropped = 2;
            m.latest_sequence = 502;
            m.ring_generation = 3;
        }

        let s = t.status(9999, 77, CAPS);
        assert_eq!(s.monitor_count, 2);
        assert_eq!(s.uptime_ms, 9999);
        assert_eq!(s.driver_build, 77);
        assert_eq!(s.max_monitors, DEFAULT_MAX_MONITORS);
        assert_eq!(s.watchdog_secs, DEFAULT_WATCHDOG_SECS);

        let m1 = s.monitors[0];
        assert_eq!(m1.session_id, 1);
        assert_eq!(m1.frames_published, 500);
        assert_eq!(m1.frames_dropped, 2);
        assert_eq!(m1.latest_sequence, 502);
        assert_eq!(m1.ring_generation, 3);
        assert_eq!((m1.width, m1.height), (1920, 1080));
        // Untouched tail entries are zero.
        assert_eq!(s.monitors[2].session_id, 0);
    }
}

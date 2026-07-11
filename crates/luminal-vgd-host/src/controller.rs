// SPDX-License-Identifier: AGPL-3.0-only
//! The capture-mode controller.
//!
//! Implements the capture ladder of DESIGN.md §2 with the 2026-07 product
//! amendment: **fallback is seamless and silent** (stream never stops, no
//! OS toast), and the controller **tries to restore direct encoding
//! mid-session, as soon as possible**, instead of waiting for the next
//! session. Restore probes back off exponentially so a flapping driver
//! can't turn the session into a probe storm, and the WGC session is kept
//! warm after a restore until direct encoding proves stable, so a relapse
//! is another seamless swap rather than a cold start.
//!
//! LuminalShine drives this object from its capture thread:
//!
//! ```text
//! probe driver ──► session_start() ──► [UseDirect | UseWgc + probes]
//! ring watch   ──► ring_signal()   ──► seamless swap + restore schedule
//! probe timer  ──► restore_probe_result() ──► [UseDirect | backoff]
//! WGC watchdog ──► wgc_trip() / rung_result() ──► R1..R6 ladder
//! ```
//!
//! Every method returns [`Action`]s in execution order; the controller
//! never performs I/O itself.

use crate::config::VgdConfig;
use crate::notice::{FallbackReason, Notice};

/// What the stream is currently captured with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureMode {
    DirectEncode,
    Wgc,
    Dda,
    Failed,
}

/// Result of probing the driver (session start or restore probe):
/// handshake + ring health, condensed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// Handshake OK, monitor creatable/created, ring ACTIVE with fresh
    /// heartbeat.
    Healthy { generation: u32 },
    Unavailable { reason: FallbackReason },
}

/// Periodic observation of the shared ring while direct encoding is
/// active (LuminalShine samples the `RingHeader` between frames).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingSignal {
    /// State ACTIVE, heartbeat fresh.
    Active { generation: u32 },
    /// State REBUILDING (TDR recovery in progress), heartbeat fresh.
    Rebuilding,
    /// Heartbeat older than `RING_HEARTBEAT_STALE_MS`: driver gone/wedged.
    HeartbeatStale,
    /// State DEAD: monitor destroyed under us.
    Dead,
    /// Ring alive but no new frame for 3× frame interval while the
    /// desktop is known-active (frame-sequence watchdog).
    Starved,
}

/// WGC recovery-ladder rungs (WGC-RELIABILITY.md §4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rung {
    R1RecreatePool,
    R2RebuildSession,
    R3RebuildDevice,
    R4CycleVirtualDisplay,
    R5DropToDda,
}

/// Ordered instructions for LuminalShine's capture thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Swap the encoder's frame source to the driver ring (generation
    /// tells the backend which texture names to open).
    UseDirect { generation: u32 },
    /// Swap the encoder's frame source to the WGC backend, targeting the
    /// LuminalVGD virtual display when present (WGC-RELIABILITY.md §1).
    UseWgc,
    /// Swap to Desktop Duplication (last resort).
    UseDda,
    /// Force an IDR/keyframe so the client resyncs instantly after a swap.
    ForceKeyframe,
    /// Call `restore_probe_result` with a fresh probe at/after this time.
    ScheduleRestoreProbe { at_ms: u64 },
    /// Direct has proven stable post-restore: release the warm WGC session.
    TeardownWgc,
    /// Execute this ladder rung, then report via `rung_result`.
    AttemptRung(Rung),
    /// R6: end the session loudly with a structured error + log bundle.
    FailSession,
    /// Deliver a status notice (host UI + log only — never an OS toast).
    Notify(Notice),
}

struct RestoreTrack {
    backoff_ms: u64,
    /// True while a WGC session is being kept warm after a restore.
    warm_wgc: bool,
    stable_frames: u32,
}

pub struct CaptureController {
    cfg: VgdConfig,
    mode: CaptureMode,
    /// Driver was present at session start (gates R4).
    driver_present: bool,
    restore: Option<RestoreTrack>,
    /// Current ladder position while a trip is being worked.
    ladder: Option<Rung>,
    ladder_reason: Option<FallbackReason>,
    last_generation: u32,
}

impl CaptureController {
    pub fn new(cfg: VgdConfig) -> Self {
        Self {
            cfg,
            mode: CaptureMode::Wgc,
            driver_present: false,
            restore: None,
            ladder: None,
            ladder_reason: None,
            last_generation: 0,
        }
    }

    pub fn mode(&self) -> CaptureMode {
        self.mode
    }

    /// Backend selection at session start — run fresh every session,
    /// never cached (DESIGN.md §2).
    pub fn session_start(&mut self, now_ms: u64, probe: ProbeOutcome) -> Vec<Action> {
        self.restore = None;
        self.ladder = None;
        self.ladder_reason = None;
        match probe {
            ProbeOutcome::Healthy { generation } => {
                self.mode = CaptureMode::DirectEncode;
                self.driver_present = true;
                self.last_generation = generation;
                vec![Action::UseDirect { generation }]
            }
            ProbeOutcome::Unavailable { reason } => {
                self.driver_present = !matches!(reason, FallbackReason::DriverAbsent);
                self.mode = CaptureMode::Wgc;
                let mut actions = vec![Action::UseWgc, Action::Notify(Notice::fell_back(reason))];
                actions.push(self.schedule_restore(now_ms));
                actions
            }
        }
    }

    /// Ring observation while direct encoding is active.
    pub fn ring_signal(&mut self, now_ms: u64, signal: RingSignal) -> Vec<Action> {
        if self.mode != CaptureMode::DirectEncode {
            return Vec::new();
        }
        match signal {
            RingSignal::Active { generation } => {
                if generation != self.last_generation {
                    // Driver rebuilt the ring fast enough that we never
                    // saw REBUILDING: re-open textures, keep streaming.
                    self.last_generation = generation;
                    vec![Action::UseDirect { generation }, Action::ForceKeyframe]
                } else {
                    Vec::new()
                }
            }
            RingSignal::Rebuilding => self.fall_back(now_ms, FallbackReason::RingRebuilding),
            RingSignal::HeartbeatStale => self.fall_back(now_ms, FallbackReason::HeartbeatStale),
            RingSignal::Dead => self.fall_back(now_ms, FallbackReason::RingDead),
            RingSignal::Starved => self.fall_back(now_ms, FallbackReason::DirectStarvation),
        }
    }

    /// The seamless, silent swap: WGC takes over between frames, client
    /// resyncs on one keyframe, LuminalShine UI (only) learns why, and the
    /// restore loop starts immediately.
    fn fall_back(&mut self, now_ms: u64, reason: FallbackReason) -> Vec<Action> {
        self.mode = CaptureMode::Wgc;
        vec![
            Action::UseWgc,
            Action::ForceKeyframe,
            Action::Notify(Notice::fell_back(reason)),
            self.schedule_restore(now_ms),
        ]
    }

    fn schedule_restore(&mut self, now_ms: u64) -> Action {
        let backoff = match &self.restore {
            Some(t) => t.backoff_ms,
            None => {
                self.restore = Some(RestoreTrack {
                    backoff_ms: self.cfg.restore_initial_backoff_ms,
                    warm_wgc: false,
                    stable_frames: 0,
                });
                self.cfg.restore_initial_backoff_ms
            }
        };
        Action::ScheduleRestoreProbe { at_ms: now_ms + backoff }
    }

    /// Outcome of a scheduled restore probe while in fallback.
    pub fn restore_probe_result(&mut self, now_ms: u64, probe: ProbeOutcome) -> Vec<Action> {
        if !matches!(self.mode, CaptureMode::Wgc | CaptureMode::Dda) {
            return Vec::new();
        }
        match probe {
            ProbeOutcome::Healthy { generation } => {
                self.mode = CaptureMode::DirectEncode;
                self.driver_present = true;
                self.last_generation = generation;
                if let Some(t) = &mut self.restore {
                    t.backoff_ms = self.cfg.restore_initial_backoff_ms;
                    t.warm_wgc = true;
                    t.stable_frames = 0;
                }
                vec![
                    Action::UseDirect { generation },
                    Action::ForceKeyframe,
                    Action::Notify(Notice::restored()),
                ]
            }
            ProbeOutcome::Unavailable { reason } => {
                let t = self.restore.get_or_insert(RestoreTrack {
                    backoff_ms: self.cfg.restore_initial_backoff_ms,
                    warm_wgc: false,
                    stable_frames: 0,
                });
                if matches!(reason, FallbackReason::DriverAbsent) {
                    self.driver_present = false;
                }
                t.backoff_ms = (t.backoff_ms * 2).min(self.cfg.restore_max_backoff_ms);
                vec![Action::ScheduleRestoreProbe { at_ms: now_ms + t.backoff_ms }]
            }
        }
    }

    /// Call once per direct frame after a mid-session restore; when direct
    /// has proven stable, the warm WGC session is released.
    pub fn direct_frame_ok(&mut self) -> Vec<Action> {
        if self.mode != CaptureMode::DirectEncode {
            return Vec::new();
        }
        if let Some(t) = &mut self.restore {
            if t.warm_wgc {
                t.stable_frames += 1;
                if t.stable_frames >= self.cfg.restore_stable_frames {
                    t.warm_wgc = false;
                    return vec![Action::TeardownWgc];
                }
            }
        }
        Vec::new()
    }

    /// WGC frame-sequence watchdog tripped (or a mode change demands an
    /// immediate R1). Starts the ladder; each rung is attempted once
    /// (WGC-RELIABILITY.md §4).
    pub fn wgc_trip(&mut self, reason: FallbackReason) -> Vec<Action> {
        if self.mode != CaptureMode::Wgc || self.ladder.is_some() {
            return Vec::new();
        }
        self.ladder = Some(Rung::R1RecreatePool);
        self.ladder_reason = Some(reason);
        vec![Action::AttemptRung(Rung::R1RecreatePool)]
    }

    /// Report a rung attempt. Success ends the trip (keyframe forced);
    /// failure advances the ladder, skipping rungs that don't apply
    /// (R4 needs the driver, R5 needs DDA enabled).
    pub fn rung_result(&mut self, rung: Rung, success: bool) -> Vec<Action> {
        if self.ladder != Some(rung) {
            return Vec::new();
        }
        if success {
            let reason = self.ladder_reason.take().unwrap_or(FallbackReason::WgcStarvation);
            self.ladder = None;
            let mut actions = vec![Action::ForceKeyframe];
            if rung == Rung::R5DropToDda {
                self.mode = CaptureMode::Dda;
                actions.push(Action::Notify(Notice::dda(reason)));
            }
            return actions;
        }
        let next = match rung {
            Rung::R1RecreatePool => Some(Rung::R2RebuildSession),
            Rung::R2RebuildSession => Some(Rung::R3RebuildDevice),
            Rung::R3RebuildDevice => {
                if self.driver_present {
                    Some(Rung::R4CycleVirtualDisplay)
                } else if self.cfg.dda_enabled {
                    Some(Rung::R5DropToDda)
                } else {
                    None
                }
            }
            Rung::R4CycleVirtualDisplay => {
                if self.cfg.dda_enabled {
                    Some(Rung::R5DropToDda)
                } else {
                    None
                }
            }
            Rung::R5DropToDda => None,
        };
        match next {
            Some(r) => {
                self.ladder = Some(r);
                vec![Action::AttemptRung(r)]
            }
            None => {
                // R6: never leave a silently frozen stream.
                self.mode = CaptureMode::Failed;
                let reason = self.ladder_reason.take().unwrap_or(FallbackReason::WgcStarvation);
                self.ladder = None;
                vec![Action::Notify(Notice::failed(reason)), Action::FailSession]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notice::{NoticeChannel, NoticeKind};

    fn ctl() -> CaptureController {
        CaptureController::new(VgdConfig::default())
    }

    fn healthy(generation: u32) -> ProbeOutcome {
        ProbeOutcome::Healthy { generation }
    }

    fn notices(actions: &[Action]) -> Vec<Notice> {
        actions
            .iter()
            .filter_map(|a| match a {
                Action::Notify(n) => Some(*n),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn healthy_driver_starts_direct_with_no_notice() {
        let mut c = ctl();
        let a = c.session_start(0, healthy(1));
        assert_eq!(a, vec![Action::UseDirect { generation: 1 }]);
        assert_eq!(c.mode(), CaptureMode::DirectEncode);
        assert!(notices(&a).is_empty(), "healthy start is not news");
    }

    #[test]
    fn absent_driver_starts_wgc_and_keeps_probing() {
        let mut c = ctl();
        let a = c.session_start(
            1000,
            ProbeOutcome::Unavailable { reason: FallbackReason::DriverAbsent },
        );
        assert_eq!(a[0], Action::UseWgc);
        assert!(matches!(a[1], Action::Notify(_)));
        assert_eq!(a[2], Action::ScheduleRestoreProbe { at_ms: 2000 });
        assert_eq!(c.mode(), CaptureMode::Wgc);
    }

    #[test]
    fn heartbeat_stale_falls_back_seamlessly_and_silently() {
        let mut c = ctl();
        c.session_start(0, healthy(1));

        let a = c.ring_signal(5_000, RingSignal::HeartbeatStale);
        // Seamless: swap source, force keyframe — the stream never stops.
        assert_eq!(a[0], Action::UseWgc);
        assert_eq!(a[1], Action::ForceKeyframe);
        // Silent: the only notice is host-UI-only with the promised copy.
        let ns = notices(&a);
        assert_eq!(ns.len(), 1);
        assert_eq!(ns[0].kind, NoticeKind::FellBackToWgc);
        assert_eq!(ns[0].channel, NoticeChannel::HostUiOnly);
        assert!(ns[0].text.contains("as soon as possible"));
        // Restore-ASAP: first probe one second out.
        assert_eq!(a[3], Action::ScheduleRestoreProbe { at_ms: 6_000 });
        assert_eq!(c.mode(), CaptureMode::Wgc);
    }

    #[test]
    fn restore_probes_back_off_exponentially_to_cap() {
        let mut c = ctl();
        c.session_start(0, healthy(1));
        c.ring_signal(0, RingSignal::Dead);

        let mut expected = vec![];
        let mut now = 1_000; // first probe fires at 0 + 1s
        let mut backoff = 1_000u64;
        for _ in 0..8 {
            let a = c.restore_probe_result(
                now,
                ProbeOutcome::Unavailable { reason: FallbackReason::DriverAbsent },
            );
            backoff = (backoff * 2).min(30_000);
            expected.push(Action::ScheduleRestoreProbe { at_ms: now + backoff });
            assert_eq!(a, vec![*expected.last().unwrap()]);
            now += backoff;
        }
        // 2s,4s,8s,16s,30s,30s,30s,30s — capped.
        assert_eq!(backoff, 30_000);
    }

    #[test]
    fn successful_probe_restores_direct_mid_session() {
        let mut c = ctl();
        c.session_start(0, healthy(1));
        c.ring_signal(10_000, RingSignal::Rebuilding);
        assert_eq!(c.mode(), CaptureMode::Wgc);

        // Driver came back with a bumped ring generation.
        let a = c.restore_probe_result(12_000, healthy(2));
        assert_eq!(
            &a[..2],
            &[Action::UseDirect { generation: 2 }, Action::ForceKeyframe]
        );
        let ns = notices(&a);
        assert_eq!(ns.len(), 1);
        assert_eq!(ns[0].kind, NoticeKind::DirectRestored);
        assert_eq!(c.mode(), CaptureMode::DirectEncode);

        // WGC stays warm until direct proves stable…
        for _ in 0..119 {
            assert!(c.direct_frame_ok().is_empty());
        }
        // …then gets torn down at the configured frame count.
        assert_eq!(c.direct_frame_ok(), vec![Action::TeardownWgc]);
        assert!(c.direct_frame_ok().is_empty(), "teardown fires once");
    }

    #[test]
    fn relapse_after_restore_swaps_back_without_cold_start() {
        let mut c = ctl();
        c.session_start(0, healthy(1));
        c.ring_signal(0, RingSignal::HeartbeatStale);
        c.restore_probe_result(1_000, healthy(2));
        // Direct relapses before the warm window closes.
        let a = c.ring_signal(2_000, RingSignal::HeartbeatStale);
        assert_eq!(a[0], Action::UseWgc);
        assert_eq!(c.mode(), CaptureMode::Wgc);
        // Backoff restarted from the configured initial value.
        assert_eq!(a[3], Action::ScheduleRestoreProbe { at_ms: 3_000 });
    }

    #[test]
    fn generation_bump_while_active_reopens_ring_without_fallback() {
        let mut c = ctl();
        c.session_start(0, healthy(1));
        // Driver rebuilt so fast we only observed the new generation.
        let a = c.ring_signal(500, RingSignal::Active { generation: 2 });
        assert_eq!(a, vec![Action::UseDirect { generation: 2 }, Action::ForceKeyframe]);
        assert_eq!(c.mode(), CaptureMode::DirectEncode);
        // Same generation again: steady state, nothing to do.
        assert!(c.ring_signal(600, RingSignal::Active { generation: 2 }).is_empty());
    }

    #[test]
    fn ladder_walks_r1_to_r6_with_driver_present() {
        let mut c = ctl();
        c.session_start(0, healthy(1));
        c.ring_signal(0, RingSignal::HeartbeatStale); // now in WGC

        let a = c.wgc_trip(FallbackReason::WgcStarvation);
        assert_eq!(a, vec![Action::AttemptRung(Rung::R1RecreatePool)]);
        let a = c.rung_result(Rung::R1RecreatePool, false);
        assert_eq!(a, vec![Action::AttemptRung(Rung::R2RebuildSession)]);
        let a = c.rung_result(Rung::R2RebuildSession, false);
        assert_eq!(a, vec![Action::AttemptRung(Rung::R3RebuildDevice)]);
        let a = c.rung_result(Rung::R3RebuildDevice, false);
        assert_eq!(a, vec![Action::AttemptRung(Rung::R4CycleVirtualDisplay)]);
        let a = c.rung_result(Rung::R4CycleVirtualDisplay, false);
        assert_eq!(a, vec![Action::AttemptRung(Rung::R5DropToDda)]);
        let a = c.rung_result(Rung::R5DropToDda, false);
        assert_eq!(a[1], Action::FailSession);
        assert_eq!(notices(&a)[0].kind, NoticeKind::SessionFailed);
        assert_eq!(c.mode(), CaptureMode::Failed);
    }

    #[test]
    fn ladder_skips_r4_without_driver_and_r5_when_dda_disabled() {
        // No driver, DDA disabled: R3 failure goes straight to R6.
        let mut cfg = VgdConfig::default();
        cfg.dda_enabled = false;
        let mut c = CaptureController::new(cfg);
        c.session_start(0, ProbeOutcome::Unavailable { reason: FallbackReason::DriverAbsent });

        c.wgc_trip(FallbackReason::WgcStarvation);
        c.rung_result(Rung::R1RecreatePool, false);
        c.rung_result(Rung::R2RebuildSession, false);
        let a = c.rung_result(Rung::R3RebuildDevice, false);
        assert_eq!(a[1], Action::FailSession);
    }

    #[test]
    fn rung_success_ends_trip_with_keyframe() {
        let mut c = ctl();
        c.session_start(0, ProbeOutcome::Unavailable { reason: FallbackReason::DriverAbsent });
        c.wgc_trip(FallbackReason::ModeChange);
        let a = c.rung_result(Rung::R1RecreatePool, true);
        assert_eq!(a, vec![Action::ForceKeyframe]);
        // Trip is over; a new trip starts from R1 again.
        let a = c.wgc_trip(FallbackReason::WgcStarvation);
        assert_eq!(a, vec![Action::AttemptRung(Rung::R1RecreatePool)]);
    }

    #[test]
    fn r5_success_moves_mode_to_dda_and_says_so() {
        let mut c = ctl();
        c.session_start(0, ProbeOutcome::Unavailable { reason: FallbackReason::DriverAbsent });
        c.wgc_trip(FallbackReason::WgcStarvation);
        c.rung_result(Rung::R1RecreatePool, false);
        c.rung_result(Rung::R2RebuildSession, false);
        c.rung_result(Rung::R3RebuildDevice, false);
        let a = c.rung_result(Rung::R5DropToDda, true);
        assert_eq!(a[0], Action::ForceKeyframe);
        assert_eq!(c.mode(), CaptureMode::Dda);
        // DDA still probes for the driver coming back.
        let a = c.restore_probe_result(60_000, healthy(1));
        assert_eq!(a[0], Action::UseDirect { generation: 1 });
    }

    #[test]
    fn stale_events_in_wrong_mode_are_ignored() {
        let mut c = ctl();
        c.session_start(0, healthy(1));
        // Ring signals only matter in direct mode; ladder only in WGC.
        assert!(c.wgc_trip(FallbackReason::WgcStarvation).is_empty());
        assert!(c.rung_result(Rung::R1RecreatePool, false).is_empty());
        c.ring_signal(0, RingSignal::Dead);
        assert!(c.ring_signal(1, RingSignal::Dead).is_empty(), "already in WGC");
        // A late probe result after restore is ignored too.
        c.restore_probe_result(2, healthy(3));
        assert!(c.restore_probe_result(3, healthy(4)).is_empty());
    }

    #[test]
    fn no_action_path_ever_raises_an_os_notification() {
        // Walk every notice-producing path and assert the channel.
        let mut c = ctl();
        let mut all = vec![];
        all.extend(c.session_start(0, ProbeOutcome::Unavailable {
            reason: FallbackReason::DriverAbsent,
        }));
        all.extend(c.wgc_trip(FallbackReason::WgcStarvation));
        all.extend(c.rung_result(Rung::R1RecreatePool, false));
        all.extend(c.rung_result(Rung::R2RebuildSession, false));
        all.extend(c.rung_result(Rung::R3RebuildDevice, false));
        all.extend(c.rung_result(Rung::R5DropToDda, true));
        all.extend(c.restore_probe_result(1_000, healthy(1)));
        all.extend(c.ring_signal(2_000, RingSignal::Starved));
        for n in notices(&all) {
            assert_eq!(n.channel, NoticeChannel::HostUiOnly);
        }
    }
}

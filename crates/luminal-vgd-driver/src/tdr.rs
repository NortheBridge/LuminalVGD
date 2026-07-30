// SPDX-License-Identifier: AGPL-3.0-only
//! TDR recovery signalling: the LOCK-FREE half of build 16's "duck the
//! device, keep the display" (DESIGN.md §3.3 rule 2).
//!
//! # Why this module exists
//!
//! When a GPU reset kills a frame worker, build 16 keeps the IddCx monitor
//! ARRIVED and starts a poller (`shell::control::device_duck_poller`) that
//! waits for the OS to re-assign a swap chain on its own. The poller needs
//! one question answered every 250 ms: **did the transport actually come
//! back?**
//!
//! Build 16's first cut answered it by `try_lock`ing the `FrameRing` mutex
//! and reading the shared section's state. That is unanswerable by
//! construction: `shell::swapchain::frame_loop` takes the FrameRing mutex
//! once and holds the guard for the WHOLE life of the worker (the
//! `let ring = &mut *ring` below it is a reborrow, not a drop). It is a
//! load-bearing invariant — it is exactly why `mark_ring_dead_arc` is a
//! bounded `try_lock` poll rather than a `lock()`. So a ring that had FULLY
//! RECOVERED — new worker, `IddCxSwapChainSetDevice` succeeded, frames
//! publishing — read as `WouldBlock`, which the poller counted as "still
//! pending". Consequences, all of them permanent:
//!
//! * the zero-modeset good exit (`TdrDeviceReassigned`) was UNREACHABLE on
//!   a genuine recovery — the entire point of build 16;
//! * ~10 s later the requalify arm departed the only active display, a
//!   modeset plus the `DBT_DEVNODES_CHANGED` broadcast, on a machine that
//!   had already healed itself;
//! * `TdrDeviceHeartbeatBlocked` fired during entirely healthy operation.
//!
//! Net: the broken discriminator made build 16 WORSE than build 15 in the
//! common case, converting self-healing recoveries into forced departures.
//!
//! # The signal
//!
//! [`RingLive`] is a lock-free mirror of the ring's transport state that
//! the frame worker PUBLISHES at its two transitions — going live (after
//! `IddCxSwapChainSetDevice` succeeded on a freshly created device) and
//! exiting — and that the poller READS without touching any mutex a live
//! worker owns. The mutex is still taken, but only to refresh the shared
//! heartbeat of a ring that the lock-free signal has already said is NOT
//! recovered; failing to get it can no longer be mistaken for "not
//! recovered", because by then we already know it is not.
//!
//! Portable on purpose: this is a driver *decision*, so per CLAUDE.md it
//! lives below the shell line and is unit-tested without Windows, the WDK,
//! or a live GPU.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use luminal_driver_proto::ring_state;

/// Lock-free mirror of one monitor's transport state.
///
/// Lives in `shell::swapchain::RingHandle` NEXT TO the `Mutex<FrameRing>`,
/// never inside it — a field behind that mutex would be exactly as
/// unreadable as the shared section was.
///
/// `state` mirrors, byte for byte, the `ring_state::*` value the driver
/// writes into the shared section header: every `RingSection::set_state`
/// call site has a `publish_state` next to it. The mirror is the one the
/// TDR poller reads, because the section itself is only reachable through
/// the mutex.
#[derive(Debug)]
pub struct RingLive {
    /// False when the shared section could not be created at plug time
    /// (transport disabled; the host is on WGC). There is no transport to
    /// restore, so such a ring is permanently settled.
    transport: bool,
    /// `ring_state::*`. Initialized ACTIVE to match `RingSection::create`.
    state: AtomicU32,
    /// A frame worker is between "SetDevice succeeded" and "returned".
    /// Not part of [`RingLive::settled`] — a worker that has just failed is
    /// still live for the microseconds it takes to return, and settling on
    /// that would walk away from the ring the duck exists to watch.
    worker_live: AtomicBool,
    /// Bumped once per go-live transition. Never reset. Diagnostics only:
    /// it is what lets a trace say "a worker really did come back" apart
    /// from "this ring was never in trouble".
    live_generation: AtomicU64,
}

impl RingLive {
    /// `transport_present` = the shared ring section exists.
    pub fn new(transport_present: bool) -> Self {
        Self {
            transport: transport_present,
            state: AtomicU32::new(ring_state::ACTIVE),
            worker_live: AtomicBool::new(false),
            live_generation: AtomicU64::new(0),
        }
    }

    /// Mirror a `ring_state::*` transition. Called next to every
    /// `RingSection::set_state`, and — deliberately — also on the paths
    /// where the bounded `try_lock` for the section LOSES: the mirror is
    /// then the only correct record of the ring's state, which is a strict
    /// improvement on losing the mark entirely.
    pub fn publish_state(&self, state: u32) {
        self.state.store(state, Ordering::Release);
    }

    /// The frame worker is live: `IddCxSwapChainSetDevice` succeeded on a
    /// freshly created D3D device and the ring is ACTIVE. Returns the new
    /// live generation. THE recovery signal — nothing else sets ACTIVE.
    pub fn publish_worker_live(&self) -> u64 {
        let generation = self.live_generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.state.store(ring_state::ACTIVE, Ordering::Release);
        self.worker_live.store(true, Ordering::Release);
        generation
    }

    /// The frame worker returned. Deliberately does NOT touch `state`: an
    /// exit is not by itself a transport failure (the routine ~10 ms
    /// unassign after every activation exits a perfectly healthy worker),
    /// and the failure exits publish REBUILDING themselves. Idempotent.
    pub fn publish_worker_exit(&self) {
        self.worker_live.store(false, Ordering::Release);
    }

    pub fn state(&self) -> u32 {
        self.state.load(Ordering::Acquire)
    }

    pub fn live_generation(&self) -> u64 {
        self.live_generation.load(Ordering::Acquire)
    }

    /// A worker is live AND the ring is ACTIVE: this display is publishing
    /// right now. The requalify arm is forbidden from departing one of
    /// these — that is the whole build-16 acceptance bar.
    pub fn publishing(&self) -> bool {
        self.worker_live.load(Ordering::Acquire) && self.state() == ring_state::ACTIVE
    }

    /// THE DISCRIMINATOR. True when the TDR poller has nothing left to
    /// watch on this ring: no transport at all, or the transport is no
    /// longer REBUILDING (ACTIVE = a worker got its device back and is
    /// publishing; DEAD = the session is finished).
    ///
    /// Lock-free by contract. Any caller that consults a mutex BEFORE this
    /// has reintroduced the build-16 blocker.
    pub fn settled(&self) -> bool {
        !self.transport || self.state() != ring_state::REBUILDING
    }
}

/// What one TDR device-duck poller tick found for a ring.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RingTick {
    /// Still REBUILDING with no worker back yet, and the shared heartbeat
    /// was refreshed so the host reads "coming back" rather than "driver
    /// gone".
    Rebuilding,
    /// Nothing left to keep alive: a worker is publishing again, the
    /// session is finished, or this ring never had a transport.
    Settled,
    /// Still REBUILDING *and* the ring mutex was unavailable, so the
    /// heartbeat could not be refreshed. Only reachable behind a
    /// deadline-detached (wedged) worker that pins the mutex for its
    /// lifetime — a LIVE worker is judged [`RingTick::Settled`] by the
    /// lock-free mirror before the lock is ever attempted.
    HeartbeatBlocked,
}

impl RingTick {
    /// Does this ring still need watching? The poller's good exit fires
    /// when nothing is pending.
    pub fn is_pending(self) -> bool {
        !matches!(self, RingTick::Settled)
    }
}

/// Decide one poller tick.
///
/// ORDER IS THE FIX: the lock-free mirror decides recovery, and
/// `lock_available` is consulted only afterwards, to say whether the
/// heartbeat of a still-REBUILDING ring could be refreshed. Asking the
/// mutex first is what made a recovered, publishing ring read as
/// permanently pending.
pub fn ring_tick(live: &RingLive, lock_available: bool) -> RingTick {
    if live.settled() {
        return RingTick::Settled;
    }
    if !lock_available {
        return RingTick::HeartbeatBlocked;
    }
    RingTick::Rebuilding
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE REGRESSION TEST for the build-16 blocker.
    ///
    /// `lock_available = false` is not a hypothetical: the frame worker
    /// holds the FrameRing guard for its entire life, so a ring that is
    /// healthy and publishing ALWAYS presents the poller with an
    /// unavailable mutex. The original discriminator asked the mutex
    /// first and returned "blocked ⇒ pending", so `pending` never reached
    /// zero, `TdrDeviceReassigned` was unreachable, and the requalify arm
    /// departed a display that had already recovered.
    #[test]
    fn recovered_ring_settles_even_though_its_worker_pins_the_mutex() {
        let live = RingLive::new(true);
        live.publish_state(ring_state::REBUILDING); // the duck armed here
        assert!(!live.settled(), "a ducked ring must start out pending");

        // The OS re-assigned a swap chain and the replacement worker got
        // its device: IddCxSwapChainSetDevice succeeded.
        let generation = live.publish_worker_live();
        assert_eq!(generation, 1);

        // That worker now owns the FrameRing mutex for as long as it runs.
        assert_eq!(ring_tick(&live, false), RingTick::Settled);
        assert!(!ring_tick(&live, false).is_pending());
        assert!(live.publishing());
    }

    /// The other half of the same bug: a ring that genuinely has NOT come
    /// back must still be watched, whether or not the mutex is available —
    /// and an unavailable mutex must be distinguishable from a healthy
    /// one, because only the former means "heartbeat lost".
    #[test]
    fn rebuilding_ring_stays_pending_and_reports_a_lost_heartbeat() {
        let live = RingLive::new(true);
        live.publish_state(ring_state::REBUILDING);
        assert_eq!(ring_tick(&live, true), RingTick::Rebuilding);
        assert!(ring_tick(&live, true).is_pending());
        // A deadline-detached corpse pins the mutex: still pending, but
        // now the ONLY thing that produces HeartbeatBlocked.
        assert_eq!(ring_tick(&live, false), RingTick::HeartbeatBlocked);
        assert!(ring_tick(&live, false).is_pending());
    }

    /// A worker exiting is not a failure — the OS unassigns ~10 ms after
    /// every activation. Only the failure exits publish REBUILDING, so a
    /// routine exit must leave the ring settled and must never drag an
    /// idle-but-healthy monitor into another session's duck.
    #[test]
    fn routine_worker_exit_leaves_the_ring_settled() {
        let live = RingLive::new(true);
        live.publish_worker_live();
        live.publish_worker_exit();
        assert!(live.settled());
        assert!(!live.publishing());
        assert_eq!(ring_tick(&live, false), RingTick::Settled);
    }

    /// The failure exits publish REBUILDING BEFORE the worker flag drops,
    /// so the ring the duck is armed against is never briefly mistaken for
    /// recovered.
    #[test]
    fn failure_exit_keeps_the_ring_pending_after_the_worker_is_gone() {
        let live = RingLive::new(true);
        live.publish_worker_live();
        live.publish_state(ring_state::REBUILDING);
        live.publish_worker_exit();
        assert!(!live.settled());
        assert!(!live.publishing());
        assert_eq!(ring_tick(&live, true), RingTick::Rebuilding);
    }

    /// DEAD (session destroyed / torn down) settles: there is no recovery
    /// to wait for. Mirrored even when the bounded try_lock for the shared
    /// section loses, which is why `mark_ring_dead_arc` publishes here
    /// unconditionally.
    #[test]
    fn dead_ring_settles() {
        let live = RingLive::new(true);
        live.publish_state(ring_state::REBUILDING);
        assert!(!live.settled());
        live.publish_state(ring_state::DEAD);
        assert!(live.settled());
        assert_eq!(ring_tick(&live, false), RingTick::Settled);
    }

    /// Section creation failed at plug (host on WGC): nothing to restore.
    #[test]
    fn transportless_ring_is_permanently_settled() {
        let live = RingLive::new(false);
        live.publish_state(ring_state::REBUILDING);
        assert!(live.settled());
        assert_eq!(ring_tick(&live, false), RingTick::Settled);
    }

    /// The live generation only counts go-live transitions, so a trace can
    /// tell "a worker really did come back" (the good exit we want) from
    /// "this ring was never REBUILDING in the first place".
    #[test]
    fn live_generation_counts_only_go_live_transitions() {
        let live = RingLive::new(true);
        assert_eq!(live.live_generation(), 0);
        live.publish_worker_live();
        live.publish_worker_exit();
        live.publish_state(ring_state::REBUILDING);
        assert_eq!(live.live_generation(), 1);
        assert_eq!(live.publish_worker_live(), 2);
    }
}

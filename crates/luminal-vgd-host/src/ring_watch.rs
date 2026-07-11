// SPDX-License-Identifier: AGPL-3.0-only
//! Ring-header interpretation: turn a sampled `RingHeader` into the
//! [`RingSignal`](crate::controller::RingSignal) the controller consumes.
//!
//! Platform-independent on purpose — the header is plain data, so the
//! fallback triggers (heartbeat staleness, rebuild, death) are testable
//! without a driver. Frame starvation (`RingSignal::Starved`) is NOT
//! decided here: it needs the encoder's frame clock and desktop-activity
//! signal, which live in LuminalShine.

use luminal_driver_proto::{ring_state, RingHeader, RING_HEARTBEAT_STALE_MS, RING_MAGIC};

use crate::controller::RingSignal;

/// Classify a sampled header. `now_qpc` must come from the same QPC domain
/// the driver stamps (i.e., `QueryPerformanceCounter` — the header carries
/// the frequency).
pub fn classify(header: &RingHeader, now_qpc: u64) -> RingSignal {
    if header.magic != RING_MAGIC || header.qpc_frequency == 0 {
        // Never-initialized or torn mapping: treat as a dead driver, not
        // a panic — the controller falls back and keeps probing.
        return RingSignal::HeartbeatStale;
    }
    let stale_ticks =
        u64::from(RING_HEARTBEAT_STALE_MS) * header.qpc_frequency / 1000;
    if now_qpc.saturating_sub(header.driver_heartbeat_qpc) > stale_ticks {
        return RingSignal::HeartbeatStale;
    }
    match header.state {
        ring_state::ACTIVE => RingSignal::Active { generation: header.ring_generation },
        ring_state::REBUILDING => RingSignal::Rebuilding,
        ring_state::DEAD => RingSignal::Dead,
        _ => RingSignal::HeartbeatStale,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use luminal_driver_proto::RING_HEADER_VERSION;

    const FREQ: u64 = 10_000_000; // 10 MHz QPC, the common value

    fn header(state: u32, heartbeat_qpc: u64, generation: u32) -> RingHeader {
        RingHeader {
            magic: RING_MAGIC,
            header_version: RING_HEADER_VERSION,
            ring_generation: generation,
            slot_count: 3,
            state,
            reserved0: 0,
            latest_sequence: 0,
            latest_present_qpc: 0,
            frames_published: 0,
            frames_dropped: 0,
            driver_heartbeat_qpc: heartbeat_qpc,
            qpc_frequency: FREQ,
        }
    }

    #[test]
    fn fresh_active_ring_reports_generation() {
        let h = header(ring_state::ACTIVE, 1_000_000, 4);
        assert_eq!(classify(&h, 1_500_000), RingSignal::Active { generation: 4 });
    }

    #[test]
    fn stale_heartbeat_wins_over_any_state() {
        // 2 s stale threshold at 10 MHz = 20M ticks.
        for st in [ring_state::ACTIVE, ring_state::REBUILDING, ring_state::DEAD] {
            let h = header(st, 0, 1);
            assert_eq!(classify(&h, 20_000_001), RingSignal::HeartbeatStale);
            // Exactly at the threshold is still considered alive.
            assert_ne!(classify(&h, 20_000_000), RingSignal::HeartbeatStale);
        }
    }

    #[test]
    fn rebuilding_and_dead_states_map_through() {
        assert_eq!(classify(&header(ring_state::REBUILDING, 5, 1), 10), RingSignal::Rebuilding);
        assert_eq!(classify(&header(ring_state::DEAD, 5, 1), 10), RingSignal::Dead);
    }

    #[test]
    fn uninitialized_or_garbage_header_is_stale_not_panic() {
        let mut h = header(ring_state::ACTIVE, 5, 1);
        h.magic = 0;
        assert_eq!(classify(&h, 10), RingSignal::HeartbeatStale);

        let mut h = header(99, 5, 1);
        h.qpc_frequency = FREQ;
        assert_eq!(classify(&h, 10), RingSignal::HeartbeatStale);

        let mut h = header(ring_state::ACTIVE, 5, 1);
        h.qpc_frequency = 0; // division guard
        assert_eq!(classify(&h, 10), RingSignal::HeartbeatStale);
    }
}

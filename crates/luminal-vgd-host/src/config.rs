// SPDX-License-Identifier: AGPL-3.0-only
//! Control options LuminalShine exposes for the `luminalvgd` backend.
//!
//! These map one-to-one onto LuminalShine config keys under the existing
//! `virtual_display_backend` selector; LuminalShine parses its config file
//! and fills this struct, so the option surface is defined exactly once on
//! the host side.

use luminal_driver_proto::{DEFAULT_RING_SLOTS, DEFAULT_WATCHDOG_SECS};

#[derive(Clone, Debug)]
pub struct VgdConfig {
    /// Preferred render adapter by DXGI description substring
    /// (SudoVDA `gpuName` equivalent). `None` = driver default
    /// (largest VRAM). Resolved to a LUID at session start.
    pub adapter_name: Option<String>,
    /// Ring slots requested at CREATE_MONITOR (clamped by the driver).
    pub ring_slots: u32,
    /// Driver watchdog seconds; informational — the effective value comes
    /// back in the handshake (driver-side registry can override).
    pub watchdog_secs: u32,
    /// Whether DXGI Desktop Duplication may be used as the last-resort
    /// rung (R5) of the recovery ladder.
    pub dda_enabled: bool,
    /// Request 2× client refresh at CREATE_MONITOR when frame generation
    /// is active (DESIGN.md §5).
    pub refresh_doubling: bool,
    /// First restore probe fires this long after a fallback.
    pub restore_initial_backoff_ms: u64,
    /// Probe interval backs off exponentially (×2) up to this cap.
    pub restore_max_backoff_ms: u64,
    /// After a successful restore, keep the WGC session warm until this
    /// many direct frames have flowed, then tear it down.
    pub restore_stable_frames: u32,
}

impl Default for VgdConfig {
    fn default() -> Self {
        Self {
            adapter_name: None,
            ring_slots: DEFAULT_RING_SLOTS,
            watchdog_secs: DEFAULT_WATCHDOG_SECS,
            dda_enabled: true,
            refresh_doubling: true,
            restore_initial_backoff_ms: 1_000,
            restore_max_backoff_ms: 30_000,
            restore_stable_frames: 120,
        }
    }
}

// SPDX-License-Identifier: AGPL-3.0-only
//! Control options LuminalShine exposes for the `luminalvgd` backend.
//!
//! These map one-to-one onto LuminalShine config keys under the existing
//! `virtual_display_backend` selector; LuminalShine parses its config file
//! and fills this struct, so the option surface is defined exactly once on
//! the host side.

use luminal_driver_proto::{DEFAULT_LEASE_TIMEOUT_MS, DEFAULT_RING_SLOTS, DEFAULT_WATCHDOG_SECS};

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
    /// is active (DESIGN.md §5). With proto v0.3 multi-mode, the doubled
    /// rate rides the same monitor as `modes[1]` — toggling frame-gen is a
    /// mode switch, not a monitor re-create.
    pub refresh_doubling: bool,
    /// Per-lease watchdog timeout requested at CREATE_MONITOR
    /// (`LEASE_TIMEOUT_USE_DEFAULT` defers to the driver's registry knob).
    pub lease_timeout_ms: u32,
    /// Keep each client's display identity stable across reconnects
    /// (`display_id` derived from the client's persistent id). `false`
    /// sends `EPHEMERAL_IDENTITY` — no Windows-remembered settings.
    pub stable_display_identity: bool,
    /// EDID physical dimensions for created monitors (0 = driver default,
    /// 600×340 mm). LuminalShine may derive these from client EDID data to
    /// match the remote panel's DPI.
    pub physical_width_mm: u32,
    pub physical_height_mm: u32,
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
            lease_timeout_ms: DEFAULT_LEASE_TIMEOUT_MS,
            stable_display_identity: true,
            physical_width_mm: 0,
            physical_height_mm: 0,
            restore_initial_backoff_ms: 1_000,
            restore_max_backoff_ms: 30_000,
            restore_stable_frames: 120,
        }
    }
}

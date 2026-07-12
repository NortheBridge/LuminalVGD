// SPDX-License-Identifier: AGPL-3.0-only
//! Permanent display pool: `count` identical always-on displays that exist
//! outside any streaming session (libvirtualdisplay behavior fold-in; the
//! modern replacement for SudoVDA's `option.txt`, which FEATURE-MATRIX.md
//! dropped). Pool displays use permanent identities, never-expiring
//! leases, and survive driver restarts via the persistence blob.

use luminal_driver_proto::{
    CreateMonitorRequest, ModeSpec, PermanentPoolConfig, LEASE_TIMEOUT_DISABLED,
    MAX_PERMANENT_DISPLAYS,
};

use crate::error::CoreError;
use crate::identity::{permanent_display_id, PERMANENT_DISPLAY_ID_BASE};
use crate::modes::Mode;

/// Lease keys for pool displays live in the permanent range too, so they
/// can never collide with host stream sessions.
pub const fn permanent_session_id(index: u32) -> u64 {
    PERMANENT_DISPLAY_ID_BASE | 0x0100_0000_0000_0000 | index as u64
}

/// Validate a pool config against caps and the monitor cap.
pub fn validate(
    config: &PermanentPoolConfig,
    drv_caps: u32,
    max_monitors: u32,
) -> Result<(), CoreError> {
    if config.count > MAX_PERMANENT_DISPLAYS || config.count > max_monitors {
        return Err(CoreError::BadPool);
    }
    if config.count > 0 {
        Mode::validate(
            config.width,
            config.height,
            config.refresh_millihz,
            config.bit_depth,
            config.hdr,
            drv_caps,
        )
        .map_err(|_| CoreError::BadPool)?;
    }
    Ok(())
}

/// The create request for pool member `index` under `config`.
pub fn member_request(config: &PermanentPoolConfig, index: u32) -> CreateMonitorRequest {
    let mut modes = [ModeSpec::default(); MAX_PERMANENT_DISPLAYS as usize];
    modes[0] = ModeSpec {
        width: config.width,
        height: config.height,
        refresh_millihz: config.refresh_millihz,
    };
    CreateMonitorRequest {
        session_id: permanent_session_id(index),
        display_id: permanent_display_id(index),
        adapter_luid: 0,
        lease_timeout_ms: LEASE_TIMEOUT_DISABLED,
        bit_depth: config.bit_depth,
        hdr: config.hdr,
        edid_serial: 0,
        flags: 0,
        mode_count: 1,
        modes,
        physical_width_mm: config.physical_width_mm,
        physical_height_mm: config.physical_height_mm,
        friendly_name: config.name,
    }
}

/// Reconcile the live pool against a desired config: which member indices
/// to destroy and which to create. Members whose settings changed are
/// recreated (destroy then create).
#[derive(Debug, PartialEq, Eq)]
pub struct Reconcile {
    pub destroy: Vec<u32>,
    pub create: Vec<u32>,
}

pub fn reconcile(
    current: &PermanentPoolConfig,
    current_count: u32,
    desired: &PermanentPoolConfig,
) -> Reconcile {
    let settings_changed = current.width != desired.width
        || current.height != desired.height
        || current.refresh_millihz != desired.refresh_millihz
        || current.bit_depth != desired.bit_depth
        || current.hdr != desired.hdr
        || current.physical_width_mm != desired.physical_width_mm
        || current.physical_height_mm != desired.physical_height_mm
        || current.name != desired.name;

    if settings_changed {
        return Reconcile {
            destroy: (0..current_count).collect(),
            create: (0..desired.count).collect(),
        };
    }
    if desired.count >= current_count {
        Reconcile { destroy: Vec::new(), create: (current_count..desired.count).collect() }
    } else {
        Reconcile { destroy: (desired.count..current_count).collect(), create: Vec::new() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use luminal_driver_proto::{caps, create_flags};

    fn config(count: u32) -> PermanentPoolConfig {
        PermanentPoolConfig {
            count,
            width: 1920,
            height: 1080,
            refresh_millihz: 60_000,
            bit_depth: 8,
            hdr: 0,
            physical_width_mm: 0,
            physical_height_mm: 0,
            name: [0; 32],
        }
    }

    #[test]
    fn validates_count_and_mode() {
        assert!(validate(&config(0), 0, 10).is_ok(), "0 disbands, always fine");
        assert!(validate(&config(4), 0, 10).is_ok());
        assert_eq!(validate(&config(5), 0, 10).err(), Some(CoreError::BadPool));
        assert_eq!(validate(&config(3), 0, 2).err(), Some(CoreError::BadPool));
        let mut bad = config(1);
        bad.width = 7;
        assert_eq!(validate(&bad, 0, 10).err(), Some(CoreError::BadPool));
        let mut hdr = config(1);
        hdr.bit_depth = 110;
        hdr.hdr = 1;
        assert_eq!(validate(&hdr, 0, 10).err(), Some(CoreError::BadPool), "caps-gated");
        assert!(validate(&hdr, caps::HDR10, 10).is_ok());
    }

    #[test]
    fn member_requests_use_permanent_identity_and_never_expire() {
        let r = member_request(&config(2), 1);
        assert_eq!(r.display_id, permanent_display_id(1));
        assert_eq!(r.session_id, permanent_session_id(1));
        assert_ne!(r.session_id, r.display_id, "lease key ≠ identity");
        assert_eq!(r.lease_timeout_ms, LEASE_TIMEOUT_DISABLED);
        assert_eq!(r.flags & create_flags::EPHEMERAL_IDENTITY, 0);
        assert_eq!(r.mode_count, 1);
    }

    #[test]
    fn reconcile_grows_shrinks_and_rebuilds() {
        // Grow 1 → 3.
        let r = reconcile(&config(1), 1, &config(3));
        assert_eq!(r, Reconcile { destroy: vec![], create: vec![1, 2] });
        // Shrink 3 → 1.
        let r = reconcile(&config(3), 3, &config(1));
        assert_eq!(r, Reconcile { destroy: vec![1, 2], create: vec![] });
        // Same count, same settings: no-op.
        let r = reconcile(&config(2), 2, &config(2));
        assert_eq!(r, Reconcile { destroy: vec![], create: vec![] });
        // Settings change: full rebuild.
        let mut newcfg = config(2);
        newcfg.refresh_millihz = 120_000;
        let r = reconcile(&config(2), 2, &newcfg);
        assert_eq!(r, Reconcile { destroy: vec![0, 1], create: vec![0, 1] });
        // Disband.
        let r = reconcile(&config(2), 2, &config(0));
        assert_eq!(r, Reconcile { destroy: vec![0, 1], create: vec![] });
    }
}

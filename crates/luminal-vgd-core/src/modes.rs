// SPDX-License-Identifier: AGPL-3.0-only
//! Exact single-entry mode validation.
//!
//! SudoVDA's key departure from generic virtual-display drivers is that a
//! created monitor advertises exactly ONE mode — the one the streaming
//! client asked for — so Windows can never "helpfully" pick something else.
//! This module owns the envelope checks for that one mode.

use luminal_driver_proto::{caps, BitDepth, ModeSpec, MAX_MODES_PER_MONITOR};

use crate::error::CoreError;

/// Supported envelope (libvirtualdisplay-parity: 320×200 floor for retro/
/// embedded clients, 8K ceiling; no policy refresh ceiling — any positive
/// millihertz the client asks for, the driver honors).
pub const MIN_WIDTH: u32 = 320;
pub const MAX_WIDTH: u32 = 7680;
pub const MIN_HEIGHT: u32 = 200;
pub const MAX_HEIGHT: u32 = 4320;
pub const MIN_REFRESH_MILLIHZ: u32 = 1;
pub const MAX_REFRESH_MILLIHZ: u32 = u32::MAX;

/// A fully validated monitor mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub refresh_millihz: u32,
    pub bit_depth: BitDepth,
    pub hdr: bool,
}

impl Mode {
    /// Validate raw wire values against the envelope and the driver's
    /// capability mask. This is the ONLY constructor — a `Mode` in hand is
    /// proof the request was acceptable.
    pub fn validate(
        width: u32,
        height: u32,
        refresh_millihz: u32,
        bit_depth_raw: u32,
        hdr_raw: u32,
        drv_caps: u32,
    ) -> Result<Self, CoreError> {
        if !(MIN_WIDTH..=MAX_WIDTH).contains(&width)
            || !(MIN_HEIGHT..=MAX_HEIGHT).contains(&height)
            || !(MIN_REFRESH_MILLIHZ..=MAX_REFRESH_MILLIHZ).contains(&refresh_millihz)
            // Encoders consume 4:2:0; odd dimensions break every one of them.
            || !width.is_multiple_of(2)
            || !height.is_multiple_of(2)
        {
            return Err(CoreError::BadMode);
        }

        let bit_depth = BitDepth::from_raw(bit_depth_raw).ok_or(CoreError::BadBitDepth)?;
        let hdr = match hdr_raw {
            0 => false,
            1 => true,
            _ => return Err(CoreError::BadMode),
        };

        // Dynamic range and depth must agree (SudoVDA's SDR 8/10 vs HDR
        // 10/12 split), and the driver must have reported the capability.
        match (hdr, bit_depth) {
            (false, BitDepth::Sdr8) => {}
            (false, BitDepth::Sdr10) => {
                if drv_caps & caps::SDR10_BIT == 0 {
                    return Err(CoreError::BadBitDepth);
                }
            }
            (true, BitDepth::Hdr10) => {
                if drv_caps & caps::HDR10 == 0 {
                    return Err(CoreError::HdrUnsupported);
                }
            }
            (true, BitDepth::Hdr12) => {
                if drv_caps & caps::HDR10 == 0 || drv_caps & caps::HDR12_BIT == 0 {
                    return Err(CoreError::HdrUnsupported);
                }
            }
            _ => return Err(CoreError::BadBitDepth),
        }

        Ok(Self { width, height, refresh_millihz, bit_depth, hdr })
    }

    /// Frame-generation refresh doubling (host-side policy, DESIGN.md §5):
    /// the doubled rate must itself fit the envelope.
    pub fn doubled_refresh(refresh_millihz: u32) -> Option<u32> {
        let doubled = refresh_millihz.checked_mul(2)?;
        (MIN_REFRESH_MILLIHZ..=MAX_REFRESH_MILLIHZ)
            .contains(&doubled)
            .then_some(doubled)
    }

    /// Validate a `CREATE_MONITOR` mode list: 1..=`MAX_MODES_PER_MONITOR`
    /// entries, every entry in-envelope, no duplicates. Bit depth / HDR /
    /// caps apply monitor-wide. `modes[0]` is preferred. Returns the
    /// validated list in request order.
    pub fn validate_list(
        specs: &[ModeSpec],
        mode_count: u32,
        bit_depth_raw: u32,
        hdr_raw: u32,
        drv_caps: u32,
    ) -> Result<Vec<Mode>, CoreError> {
        let count = mode_count as usize;
        if count == 0 || count > MAX_MODES_PER_MONITOR as usize || count > specs.len() {
            return Err(CoreError::BadMode);
        }
        let mut out = Vec::with_capacity(count);
        for spec in &specs[..count] {
            let mode = Mode::validate(
                spec.width,
                spec.height,
                spec.refresh_millihz,
                bit_depth_raw,
                hdr_raw,
                drv_caps,
            )?;
            if out.contains(&mode) {
                return Err(CoreError::BadMode);
            }
            out.push(mode);
        }
        Ok(out)
    }

    /// Additive merge for `UPDATE_MODES` (proto 0.5): return `current`
    /// with every entry of `added` that is not already advertised
    /// appended, capped at `MAX_MODES_PER_MONITOR`.
    ///
    /// Append-only is the whole safety argument for changing the mode list
    /// of a LIVE monitor, and every clause below is load-bearing:
    ///
    /// - `current[0]` is never displaced, so the EDID's preferred detailed
    ///   timing (frozen at `IddCxMonitorCreate`, and NOT reissuable
    ///   afterwards) keeps describing the mode the driver still calls
    ///   preferred, and `PreferredMonitorModeIdx = 0` keeps meaning what
    ///   the OS was told at arrival.
    /// - No entry is ever removed, so the mode the OS currently has
    ///   COMMITTED is still in the list after the update. The driver
    ///   cannot identify the committed mode (`EvtIddCxMonitorCommitModes2`
    ///   stores nothing), so "never drop anything" is the only way to
    ///   guarantee the update does not invalidate it — which is what keeps
    ///   an update from forcing a modeset on a live stream.
    /// - Duplicates collapse, so a host that resends its full desired list
    ///   every time is idempotent rather than cap-exhausting.
    ///
    /// Returns `(merged, added_count)`. `added_count == 0` means the
    /// update is a no-op and the caller should not disturb the OS at all.
    pub fn merge_additive(current: &[Mode], added: &[Mode]) -> (Vec<Mode>, usize) {
        let mut out = current.to_vec();
        let mut appended = 0usize;
        for mode in added {
            if out.len() >= MAX_MODES_PER_MONITOR as usize {
                break;
            }
            if out.contains(mode) {
                continue;
            }
            out.push(*mode);
            appended += 1;
        }
        (out, appended)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_CAPS: u32 =
        caps::HDR10 | caps::HDR12_BIT | caps::SDR10_BIT | caps::DIRTY_RECTS | caps::REFRESH_DOUBLING;

    #[test]
    fn accepts_common_streaming_modes() {
        for (w, h, hz) in [
            (1920, 1080, 60_000),
            (2560, 1440, 120_000),
            (3840, 2160, 119_880), // 119.88 Hz fractional
            (7680, 4320, 60_000),
            (1280, 800, 90_000), // handheld
        ] {
            let m = Mode::validate(w, h, hz, 8, 0, ALL_CAPS).unwrap();
            assert_eq!((m.width, m.height, m.refresh_millihz), (w, h, hz));
        }
    }

    #[test]
    fn rejects_out_of_envelope() {
        assert_eq!(Mode::validate(318, 240, 60_000, 8, 0, ALL_CAPS), Err(CoreError::BadMode));
        assert_eq!(Mode::validate(640, 198, 60_000, 8, 0, ALL_CAPS), Err(CoreError::BadMode));
        assert_eq!(Mode::validate(7682, 4320, 60_000, 8, 0, ALL_CAPS), Err(CoreError::BadMode));
        assert_eq!(Mode::validate(1920, 1080, 0, 8, 0, ALL_CAPS), Err(CoreError::BadMode));
        // Odd dimensions.
        assert_eq!(Mode::validate(1921, 1080, 60_000, 8, 0, ALL_CAPS), Err(CoreError::BadMode));
        assert_eq!(Mode::validate(1920, 1081, 60_000, 8, 0, ALL_CAPS), Err(CoreError::BadMode));
        // Junk hdr flag.
        assert_eq!(Mode::validate(1920, 1080, 60_000, 8, 7, ALL_CAPS), Err(CoreError::BadMode));
        // No policy refresh ceiling anymore (libvirtualdisplay parity).
        assert!(Mode::validate(320, 200, 60_000, 8, 0, ALL_CAPS).is_ok());
        assert!(Mode::validate(1920, 1080, 1_000_000, 8, 0, ALL_CAPS).is_ok());
    }

    #[test]
    fn depth_and_dynamic_range_must_agree() {
        // HDR flag with SDR depth and vice versa.
        assert_eq!(Mode::validate(1920, 1080, 60_000, 8, 1, ALL_CAPS), Err(CoreError::BadBitDepth));
        assert_eq!(
            Mode::validate(1920, 1080, 60_000, 110, 0, ALL_CAPS),
            Err(CoreError::BadBitDepth)
        );
        assert_eq!(
            Mode::validate(1920, 1080, 60_000, 12, 1, ALL_CAPS),
            Err(CoreError::BadBitDepth)
        );
    }

    #[test]
    fn caps_gate_hdr_and_deep_color() {
        // No HDR cap: HDR10 refused with the specific reason.
        assert_eq!(
            Mode::validate(1920, 1080, 60_000, 110, 1, caps::SDR10_BIT),
            Err(CoreError::HdrUnsupported)
        );
        // HDR10 cap but no 12-bit cap: HDR12 refused.
        assert_eq!(
            Mode::validate(1920, 1080, 60_000, 112, 1, caps::HDR10),
            Err(CoreError::HdrUnsupported)
        );
        // SDR10 needs its cap.
        assert_eq!(
            Mode::validate(1920, 1080, 60_000, 10, 0, caps::HDR10),
            Err(CoreError::BadBitDepth)
        );
        // With the right caps all pass.
        assert!(Mode::validate(1920, 1080, 60_000, 110, 1, ALL_CAPS).is_ok());
        assert!(Mode::validate(1920, 1080, 60_000, 112, 1, ALL_CAPS).is_ok());
        assert!(Mode::validate(1920, 1080, 60_000, 10, 0, ALL_CAPS).is_ok());
    }

    #[test]
    fn refresh_doubling_respects_envelope() {
        assert_eq!(Mode::doubled_refresh(60_000), Some(120_000));
        assert_eq!(Mode::doubled_refresh(240_000), Some(480_000));
        assert_eq!(Mode::doubled_refresh(u32::MAX), None); // overflow
    }

    #[test]
    fn mode_lists_validate_as_a_set() {
        let specs = [
            ModeSpec { width: 2560, height: 1440, refresh_millihz: 120_000 },
            ModeSpec { width: 2560, height: 1440, refresh_millihz: 240_000 }, // fg-doubled
            ModeSpec::default(),
            ModeSpec::default(),
        ];
        let modes = Mode::validate_list(&specs, 2, 8, 0, ALL_CAPS).unwrap();
        assert_eq!(modes.len(), 2);
        assert_eq!(modes[0].refresh_millihz, 120_000, "preferred first");

        // Zero or too many entries.
        assert_eq!(Mode::validate_list(&specs, 0, 8, 0, ALL_CAPS).err(), Some(CoreError::BadMode));
        assert_eq!(Mode::validate_list(&specs, 5, 8, 0, ALL_CAPS).err(), Some(CoreError::BadMode));
        // A bad entry anywhere fails the list (entry 2 is 0×0).
        assert_eq!(Mode::validate_list(&specs, 3, 8, 0, ALL_CAPS).err(), Some(CoreError::BadMode));
        // Duplicates rejected.
        let dup = [specs[0], specs[0]];
        assert_eq!(Mode::validate_list(&dup, 2, 8, 0, ALL_CAPS).err(), Some(CoreError::BadMode));
    }

    fn m(hz: u32) -> Mode {
        Mode::validate(2560, 1440, hz, 8, 0, ALL_CAPS).unwrap()
    }

    /// Build 17: the merge that makes a live mode list growable. The
    /// motivating case is the whole reason the opcode exists — a monitor
    /// created at the base rate for a Moonlight desktop stream, then a
    /// frame-generation game launches and the doubled rate has to become
    /// available WITHOUT a destroy/create cycle.
    #[test]
    fn additive_merge_adds_the_framegen_rate_without_disturbing_the_live_one() {
        let live = vec![m(120_000)];
        let (merged, added) = Mode::merge_additive(&live, &[m(240_000)]);
        assert_eq!(added, 1);
        assert_eq!(merged, vec![m(120_000), m(240_000)]);
        // Preferred timing untouched: the EDID still describes modes[0].
        assert_eq!(merged[0], live[0]);
    }

    #[test]
    fn additive_merge_never_removes_and_never_reorders() {
        let live = vec![m(120_000), m(240_000)];
        // A host asking for ONLY the doubled rate cannot drop the base
        // rate — the OS may have it committed right now.
        let (merged, added) = Mode::merge_additive(&live, &[m(240_000)]);
        assert_eq!((merged.as_slice(), added), (live.as_slice(), 0));
        // Nor can it promote a later mode to preferred.
        let (merged, added) = Mode::merge_additive(&live, &[m(240_000), m(60_000)]);
        assert_eq!(merged, vec![m(120_000), m(240_000), m(60_000)]);
        assert_eq!(added, 1);
        assert_eq!(merged[0], live[0]);
    }

    #[test]
    fn additive_merge_is_idempotent_and_capped() {
        let live = vec![m(120_000)];
        let want = [m(120_000), m(240_000)];
        let (once, added_once) = Mode::merge_additive(&live, &want);
        let (twice, added_twice) = Mode::merge_additive(&once, &want);
        assert_eq!(once, twice, "resending the same desired list changes nothing");
        assert_eq!((added_once, added_twice), (1, 0));

        // At the cap, extra entries are dropped rather than displacing
        // anything already advertised.
        let full = vec![m(60_000), m(90_000), m(120_000), m(240_000)];
        assert_eq!(full.len(), MAX_MODES_PER_MONITOR as usize);
        let (merged, added) = Mode::merge_additive(&full, &[m(30_000)]);
        assert_eq!((merged, added), (full, 0));
    }
}

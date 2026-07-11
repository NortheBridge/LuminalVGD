// SPDX-License-Identifier: AGPL-3.0-only
//! Exact single-entry mode validation.
//!
//! SudoVDA's key departure from generic virtual-display drivers is that a
//! created monitor advertises exactly ONE mode — the one the streaming
//! client asked for — so Windows can never "helpfully" pick something else.
//! This module owns the envelope checks for that one mode.

use luminal_driver_proto::{caps, BitDepth};

use crate::error::CoreError;

/// Supported envelope (SudoVDA lineage: up to 8K / high refresh).
pub const MIN_WIDTH: u32 = 640;
pub const MAX_WIDTH: u32 = 7680;
pub const MIN_HEIGHT: u32 = 480;
pub const MAX_HEIGHT: u32 = 4320;
/// 23.000 Hz floor covers 23.976 film rates; 480 Hz ceiling leaves room
/// for frame-generation doubling of 240 Hz panels.
pub const MIN_REFRESH_MILLIHZ: u32 = 23_000;
pub const MAX_REFRESH_MILLIHZ: u32 = 480_000;

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
        if width < MIN_WIDTH
            || width > MAX_WIDTH
            || height < MIN_HEIGHT
            || height > MAX_HEIGHT
            || refresh_millihz < MIN_REFRESH_MILLIHZ
            || refresh_millihz > MAX_REFRESH_MILLIHZ
            // Encoders consume 4:2:0; odd dimensions break every one of them.
            || width % 2 != 0
            || height % 2 != 0
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
        assert_eq!(Mode::validate(320, 240, 60_000, 8, 0, ALL_CAPS), Err(CoreError::BadMode));
        assert_eq!(Mode::validate(7682, 4320, 60_000, 8, 0, ALL_CAPS), Err(CoreError::BadMode));
        assert_eq!(Mode::validate(1920, 1080, 10_000, 8, 0, ALL_CAPS), Err(CoreError::BadMode));
        assert_eq!(Mode::validate(1920, 1080, 500_000, 8, 0, ALL_CAPS), Err(CoreError::BadMode));
        // Odd dimensions.
        assert_eq!(Mode::validate(1921, 1080, 60_000, 8, 0, ALL_CAPS), Err(CoreError::BadMode));
        assert_eq!(Mode::validate(1920, 1081, 60_000, 8, 0, ALL_CAPS), Err(CoreError::BadMode));
        // Junk hdr flag.
        assert_eq!(Mode::validate(1920, 1080, 60_000, 8, 7, ALL_CAPS), Err(CoreError::BadMode));
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
        assert_eq!(Mode::doubled_refresh(241_000), None); // would exceed 480 Hz
    }
}

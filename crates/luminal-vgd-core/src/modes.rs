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

    /// Resolve an `UPDATE_MODES` request (proto 0.5) into the TARGET-mode
    /// list to publish: every entry of `requested` that has a compatible
    /// entry in `supported` — the monitor-mode SUPERSET fixed at
    /// `IddCxMonitorCreate` — in request order, deduplicated.
    ///
    /// # Why this is a filter and not a merge (build 17, second model)
    ///
    /// `IDARG_IN_UPDATEMODES2` (IddCx.h:3586) carries `{ Reason,
    /// TargetModeCount, pTargetModes }` — TARGET modes, nothing else.
    /// There is no DDI anywhere in IddCx 1.10 or 1.11 that replaces an
    /// arrived monitor's DESCRIPTION, so its MONITOR-mode set is frozen at
    /// create; and Windows presents the INTERSECTION of the monitor-mode
    /// list with the target-mode list (the intersection is skipped only
    /// for remote drivers setting
    /// `IDDCX_ADAPTER_FLAGS_REMOTE_ALL_TARGET_MODES_MONITOR_COMPATIBLE`,
    /// which a console-session driver cannot be). So a target this driver
    /// publishes with no counterpart in `supported` is invisible to the
    /// user no matter what the OS returns — it is a lie the driver would
    /// then have to explain. Filtering here is what makes the reply
    /// truthful.
    ///
    /// # The safety property
    ///
    /// `targets ⊆ supported`, always. That single containment is what
    /// makes the published list safe under EITHER possible semantics of
    /// `IddCxMonitorUpdateModes2` (replace or append — undocumented, see
    /// `shell::monitors::push_targets`): a replace leaves the OS holding
    /// `targets`, an append leaves it holding `previous ∪ targets`, and
    /// every list this driver ever publishes is a subset of the same fixed
    /// superset — so the intersection is non-empty and every mode in it is
    /// one the monitor really describes. Windows can always activate
    /// something.
    ///
    /// A request whose entries are ALL outside `supported` yields an empty
    /// `targets`, which the caller must refuse rather than publish:
    /// IddCx.h:3594 states `TargetModeCount` "cannot be zero", and a
    /// monitor with no targets is a monitor with nothing to activate.
    /// [`TargetSelection::is_empty`] is that check.
    pub fn select_targets(supported: &[Mode], requested: &[Mode]) -> TargetSelection {
        let mut targets: Vec<Mode> = Vec::with_capacity(requested.len());
        let mut accepted = 0usize;
        let mut rejected = 0usize;
        let mut first_rejected = None;
        for (i, mode) in requested.iter().enumerate() {
            if !supported.contains(mode) {
                // Outside the superset established at create. Counted and
                // located, never silently dropped: a caller told only
                // "fewer than you asked for" cannot tell which mode the
                // monitor will never offer, and that is precisely the
                // fact it needs to stop offering it to a client.
                rejected += 1;
                first_rejected.get_or_insert(i);
                continue;
            }
            // A duplicate is already published by this very list, so it
            // counts as accepted — that keeps `accepted + rejected ==
            // requested.len()` true for every input, which is the
            // invariant the reply's partial-application test rests on.
            if !targets.contains(mode) {
                targets.push(*mode);
            }
            accepted += 1;
        }
        TargetSelection { targets, accepted, rejected, first_rejected }
    }
}

/// The outcome of [`Mode::select_targets`].
///
/// `accepted + rejected == requested.len()` always: every requested entry
/// either has a compatible monitor mode (and is therefore published) or
/// does not, never both and never neither.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetSelection {
    /// The target list to publish, in request order, deduplicated. A
    /// subset of the monitor superset by construction. **Never publish
    /// this when it is empty** — see [`is_empty`](Self::is_empty).
    pub targets: Vec<Mode>,
    /// Requested entries the monitor can really offer.
    pub accepted: usize,
    /// Requested entries with no compatible entry in the monitor
    /// superset. `rejected > 0` with `accepted > 0` is PARTIAL
    /// application: the modes that exist are published and the rest are
    /// reported — never a failed request, never a failed session.
    pub rejected: usize,
    /// Index into the request of the first rejected entry, so a caller can
    /// name the offending mode instead of just counting it.
    pub first_rejected: Option<usize>,
}

impl TargetSelection {
    /// Nothing in the request exists on this monitor. The caller must
    /// refuse the request with detail and leave the published list alone:
    /// `TargetModeCount` cannot be zero (IddCx.h:3594), and an empty
    /// target list would leave the monitor with nothing to activate.
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
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

    /// Build 17: the target selection that makes a live monitor's
    /// ADVERTISED set steerable. The motivating case is the whole reason
    /// the opcode exists — a monitor created with both the base rate and
    /// the frame-generation-doubled rate in its superset, streaming at the
    /// base rate, and then a framegen title launches and only the doubled
    /// rate should be on offer, WITHOUT a destroy/create cycle.
    #[test]
    fn select_targets_publishes_the_requested_subset_of_the_superset() {
        let superset = vec![m(120_000), m(240_000)];
        let r = Mode::select_targets(&superset, &[m(240_000)]);
        assert_eq!((r.accepted, r.rejected, r.first_rejected), (1, 0, None));
        assert_eq!(r.targets, vec![m(240_000)]);
        assert!(!r.is_empty());

        // ...and back again. A replace can steer in both directions; an
        // append-only merge structurally could not.
        let r = Mode::select_targets(&superset, &[m(120_000)]);
        assert_eq!(r.targets, vec![m(120_000)]);

        // Order is the request's, not the superset's: the host decides
        // which target it puts first.
        let r = Mode::select_targets(&superset, &[m(240_000), m(120_000)]);
        assert_eq!(r.targets, vec![m(240_000), m(120_000)]);
        assert_eq!((r.accepted, r.rejected), (2, 0));
    }

    /// The containment that makes the publish safe whichever way
    /// `IddCxMonitorUpdateModes2` behaves: a target with no monitor mode
    /// behind it can never reach the OS, so the target list is always a
    /// subset of the frozen superset and the intersection Windows presents
    /// is always non-empty and always activatable.
    #[test]
    fn select_targets_rejects_anything_outside_the_superset_with_detail() {
        let superset = vec![m(60_000), m(120_000)];
        // The classic mistake this model exists to make impossible:
        // "advertise a rate the monitor description never described".
        let r = Mode::select_targets(&superset, &[m(120_000), m(240_000)]);
        assert_eq!(r.targets, vec![m(120_000)], "only what the monitor really has");
        assert_eq!((r.accepted, r.rejected), (1, 1));
        assert_eq!(r.first_rejected, Some(1), "and WHICH entry was refused");
        assert!(!r.is_empty(), "partial: publish what exists");

        for t in &r.targets {
            assert!(superset.contains(t), "targets ⊆ superset, always");
        }
    }

    /// Everything asked for is outside the superset. The selection is
    /// empty, and an empty target list must never be published:
    /// IddCx.h:3594 says TargetModeCount "cannot be zero", and a monitor
    /// with no targets has nothing Windows can activate.
    #[test]
    fn select_targets_reports_an_empty_selection_rather_than_an_empty_list() {
        let superset = vec![m(60_000)];
        let r = Mode::select_targets(&superset, &[m(120_000), m(240_000)]);
        assert!(r.is_empty(), "the caller must refuse, not publish");
        assert_eq!((r.accepted, r.rejected, r.first_rejected), (0, 2, Some(0)));

        // Degenerate input: an empty request selects nothing, and is
        // likewise refusable rather than publishable.
        let r = Mode::select_targets(&superset, &[]);
        assert!(r.is_empty());
        assert_eq!((r.accepted, r.rejected), (0, 0));
    }

    #[test]
    fn select_targets_is_idempotent_and_counts_every_requested_entry() {
        let superset = vec![m(60_000), m(90_000), m(120_000), m(240_000)];
        assert_eq!(superset.len(), MAX_MODES_PER_MONITOR as usize);
        let want = [m(120_000), m(240_000)];
        let once = Mode::select_targets(&superset, &want);
        let twice = Mode::select_targets(&superset, &once.targets);
        assert_eq!(once.targets, twice.targets, "resending the published list changes nothing");

        // A duplicate is already published by this list: accepted, listed
        // once. The whole superset is publishable in one go.
        let r = Mode::select_targets(&superset, &[m(60_000), m(60_000)]);
        assert_eq!((r.targets, r.accepted, r.rejected), (vec![m(60_000)], 2, 0));
        let r = Mode::select_targets(&superset, &superset);
        assert_eq!(r.targets, superset);

        // The invariant every caller relies on to report honestly.
        for req in [
            vec![m(30_000)],
            vec![m(30_000), m(45_000)],
            vec![m(120_000)],
            vec![m(120_000), m(30_000), m(240_000)],
        ] {
            let r = Mode::select_targets(&superset, &req);
            assert_eq!(r.accepted + r.rejected, req.len());
        }
    }
}

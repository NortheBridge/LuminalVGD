// SPDX-License-Identifier: AGPL-3.0-only
//! Per-monitor EDID 1.4 generation.
//!
//! SudoVDA shipped one static high-res EDID blob for every monitor; we
//! generate a fresh 128-byte base block per created monitor instead
//! (FEATURE-MATRIX.md "High-res EDID"), so the preferred detailed timing,
//! display name, and serial all match the session exactly. Windows keys
//! per-display settings on (manufacturer, product, serial) — a distinct
//! `edid_serial` per client keeps their display settings from colliding,
//! which is the SudoVDA behavior LuminalShine relies on.
//!
//! Scope notes:
//! - A Detailed Timing Descriptor physically caps at 4095×4095 active
//!   pixels and a 655.35 MHz pixel clock. Modes beyond that (8K, 4K@120+)
//!   get a 3840×2160@60 (or best-fitting) DTD stand-in; the *actual* mode
//!   list a monitor advertises to Windows always comes from the IddCx
//!   mode list, which carries the exact session mode regardless of what
//!   the DTD says. The EDID is identity + hint, never the mode authority.
//! - HDR mastering data travels per-frame in `SlotMetadata.hdr` and via
//!   IddCx HDR DDIs, not via a CTA-861 extension block; extension count
//!   is 0. (Revisit if a future consumer demands EDID-borne HDR caps.)

use luminal_driver_proto::BitDepth;

use crate::modes::Mode;

/// Fixed blanking model used for generated DTDs (conventional values;
/// virtual displays have no analog timing constraints to honor).
const HBLANK: u32 = 160;
const VBLANK: u32 = 45;
const HSYNC_OFFSET: u32 = 48;
const HSYNC_WIDTH: u32 = 32;
const VSYNC_OFFSET: u32 = 3;
const VSYNC_WIDTH: u32 = 5;

/// DTD hard limits (EDID 1.4 structure).
const DTD_MAX_ACTIVE: u32 = 4095;
const DTD_MAX_CLOCK_10KHZ: u64 = 65_535;

/// Which timing the DTD ended up describing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtdTiming {
    /// DTD carries the exact session mode.
    Exact,
    /// Session mode exceeds DTD limits; DTD carries this stand-in
    /// (width, height, refresh_millihz). IddCx mode list still carries
    /// the exact mode.
    StandIn(u32, u32, u32),
}

pub struct Edid {
    pub bytes: [u8; 128],
    pub dtd: DtdTiming,
}

/// Pixel clock in 10 kHz units for a mode under the fixed blanking model,
/// rounded to nearest.
fn clock_10khz(width: u32, height: u32, refresh_millihz: u32) -> u64 {
    let hz_x1000 =
        u64::from(width + HBLANK) * u64::from(height + VBLANK) * u64::from(refresh_millihz);
    (hz_x1000 + 5_000_000) / 10_000_000
}

fn dtd_fits(width: u32, height: u32, refresh_millihz: u32) -> bool {
    width <= DTD_MAX_ACTIVE
        && height <= DTD_MAX_ACTIVE
        && clock_10khz(width, height, refresh_millihz) <= DTD_MAX_CLOCK_10KHZ
}

/// Choose what the DTD will describe for a session mode.
fn choose_dtd(mode: &Mode) -> DtdTiming {
    if dtd_fits(mode.width, mode.height, mode.refresh_millihz) {
        return DtdTiming::Exact;
    }
    // Keep the resolution if only the clock is the problem (halve refresh
    // until it fits); otherwise fall to 4K, then 1080p.
    let mut hz = mode.refresh_millihz / 2;
    while hz >= crate::modes::MIN_REFRESH_MILLIHZ
        && mode.width <= DTD_MAX_ACTIVE
        && mode.height <= DTD_MAX_ACTIVE
    {
        if dtd_fits(mode.width, mode.height, hz) {
            return DtdTiming::StandIn(mode.width, mode.height, hz);
        }
        hz /= 2;
    }
    if dtd_fits(3840, 2160, 60_000) {
        DtdTiming::StandIn(3840, 2160, 60_000)
    } else {
        DtdTiming::StandIn(1920, 1080, 60_000) // unreachable safety net
    }
}

fn write_dtd(out: &mut [u8], width: u32, height: u32, refresh_millihz: u32) {
    let clock = clock_10khz(width, height, refresh_millihz);
    debug_assert!(clock <= DTD_MAX_CLOCK_10KHZ);
    out[0] = (clock & 0xFF) as u8;
    out[1] = (clock >> 8) as u8;
    out[2] = (width & 0xFF) as u8;
    out[3] = (HBLANK & 0xFF) as u8;
    out[4] = (((width >> 8) as u8) << 4) | ((HBLANK >> 8) as u8);
    out[5] = (height & 0xFF) as u8;
    out[6] = (VBLANK & 0xFF) as u8;
    out[7] = (((height >> 8) as u8) << 4) | ((VBLANK >> 8) as u8);
    out[8] = HSYNC_OFFSET as u8;
    out[9] = HSYNC_WIDTH as u8;
    out[10] = ((VSYNC_OFFSET as u8) << 4) | (VSYNC_WIDTH as u8);
    out[11] = 0; // all high bits zero at these values
    out[12] = 0; // physical size unknown (virtual display)
    out[13] = 0;
    out[14] = 0;
    out[15] = 0; // no border
    out[16] = 0;
    out[17] = 0x1E; // non-interlaced, digital separate sync, +h +v
}

/// 13-byte text descriptor payload: ASCII, 0x0A terminator, 0x20 padding.
fn write_text_descriptor(out: &mut [u8], tag: u8, text: &str) {
    out[0] = 0;
    out[1] = 0;
    out[2] = 0;
    out[3] = tag;
    out[4] = 0;
    let mut i = 0;
    for ch in text.chars().take(13) {
        out[5 + i] = if ch.is_ascii_graphic() || ch == ' ' { ch as u8 } else { b'?' };
        i += 1;
    }
    if i < 13 {
        out[5 + i] = 0x0A;
        i += 1;
        while i < 13 {
            out[5 + i] = 0x20;
            i += 1;
        }
    }
}

/// Generate the 128-byte EDID base block for a created monitor.
///
/// `friendly_name` is the NUL-padded UTF-16 name from
/// `CreateMonitorRequest`; `serial` is `edid_serial`.
pub fn generate(mode: &Mode, friendly_name: &[u16], serial: u32) -> Edid {
    let mut e = [0u8; 128];

    // Header.
    e[0..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
    // Manufacturer "NBF" (NortheBridge Foundation), big-endian 5-bit letters.
    e[8] = 0x38;
    e[9] = 0x46;
    // Product code "VD" (LE).
    e[10] = 0x44;
    e[11] = 0x56;
    // Serial (LE) — per-client, keeps Windows display settings distinct.
    e[12..16].copy_from_slice(&serial.to_le_bytes());
    e[16] = 0; // week unspecified
    e[17] = 36; // 2026
    e[18] = 1; // EDID 1.4
    e[19] = 4;
    // Video input: digital, color depth per mode, DisplayPort interface.
    e[20] = match mode.bit_depth {
        BitDepth::Sdr8 => 0xA5,
        BitDepth::Sdr10 | BitDepth::Hdr10 => 0xB5,
        BitDepth::Hdr12 => 0xC5,
    };
    e[21] = 0; // screen size unknown
    e[22] = 0;
    e[23] = 120; // gamma 2.2
    e[24] = 0x02; // preferred timing includes native format
    // sRGB chromaticity.
    e[25..35].copy_from_slice(&[0xEE, 0x91, 0xA3, 0x54, 0x4C, 0x99, 0x26, 0x0F, 0x50, 0x54]);
    // No established timings.
    // Standard timings unused.
    for i in (38..54).step_by(2) {
        e[i] = 0x01;
        e[i + 1] = 0x01;
    }

    // Descriptor 1: preferred detailed timing.
    let dtd = choose_dtd(mode);
    let (dw, dh, dhz) = match dtd {
        DtdTiming::Exact => (mode.width, mode.height, mode.refresh_millihz),
        DtdTiming::StandIn(w, h, hz) => (w, h, hz),
    };
    write_dtd(&mut e[54..72], dw, dh, dhz);

    // Descriptor 2: display product name.
    let name: String = char::decode_utf16(
        friendly_name.iter().copied().take_while(|&c| c != 0),
    )
    .map(|r| r.unwrap_or('?'))
    .collect();
    let name = if name.is_empty() { "Luminal VGD" } else { name.as_str() };
    write_text_descriptor(&mut e[72..90], 0xFC, name);

    // Descriptor 3: serial string (hex, matches byte 12 serial).
    let serial_text = format!("LVGD{serial:08X}");
    write_text_descriptor(&mut e[90..108], 0xFF, &serial_text);

    // Descriptor 4: dummy.
    e[108..126].fill(0);
    e[111] = 0x10;

    e[126] = 0; // no extension blocks (see module docs re: HDR)
    let sum: u32 = e[..127].iter().map(|&b| u32::from(b)).sum();
    e[127] = ((256 - (sum % 256)) % 256) as u8;

    Edid { bytes: e, dtd }
}

#[cfg(test)]
mod tests {
    use super::*;
    use luminal_driver_proto::caps;

    fn mode(w: u32, h: u32, hz: u32) -> Mode {
        Mode::validate(w, h, hz, 8, 0, caps::SDR10_BIT).unwrap()
    }

    fn utf16(s: &str) -> Vec<u16> {
        let mut v: Vec<u16> = s.encode_utf16().collect();
        v.push(0);
        v
    }

    #[test]
    fn checksum_and_header_valid() {
        let e = generate(&mode(1920, 1080, 60_000), &utf16("Living Room"), 42);
        assert_eq!(&e.bytes[0..8], &[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        let sum: u32 = e.bytes.iter().map(|&b| u32::from(b)).sum();
        assert_eq!(sum % 256, 0, "EDID checksum must zero the block");
        assert_eq!(e.bytes[126], 0, "no extension blocks");
    }

    #[test]
    fn dtd_encodes_exact_1080p60() {
        let e = generate(&mode(1920, 1080, 60_000), &utf16("X"), 1);
        assert_eq!(e.dtd, DtdTiming::Exact);
        // (1920+160)*(1080+45)*60 Hz = 140.4 MHz => 14040 * 10 kHz.
        let clock = u16::from_le_bytes([e.bytes[54], e.bytes[55]]);
        assert_eq!(clock, 14040);
        // Active pixels round-trip.
        let hactive = u32::from(e.bytes[56]) | (u32::from(e.bytes[58] >> 4) << 8);
        let vactive = u32::from(e.bytes[59]) | (u32::from(e.bytes[61] >> 4) << 8);
        assert_eq!((hactive, vactive), (1920, 1080));
    }

    #[test]
    fn fractional_refresh_rounds_clock() {
        let e = generate(&mode(1920, 1080, 59_940), &utf16("X"), 1);
        let clock = u16::from_le_bytes([e.bytes[54], e.bytes[55]]);
        // 2080*1125*59.94 Hz = 140.2596 MHz => rounds to 14026.
        assert_eq!(clock, 14026);
    }

    #[test]
    fn oversized_modes_get_stand_in_dtd() {
        // 8K: active exceeds the 4095 DTD cap entirely => 4K60 stand-in.
        let e = generate(&mode(7680, 4320, 60_000), &utf16("X"), 1);
        assert_eq!(e.dtd, DtdTiming::StandIn(3840, 2160, 60_000));

        // 4K120: active fits but clock (1058 MHz) doesn't => same res,
        // halved refresh.
        let e = generate(&mode(3840, 2160, 120_000), &utf16("X"), 1);
        assert_eq!(e.dtd, DtdTiming::StandIn(3840, 2160, 60_000));
    }

    #[test]
    fn serial_and_name_are_stamped() {
        let e = generate(&mode(1920, 1080, 60_000), &utf16("Steam Deck OLED extra"), 0xBEEF);
        assert_eq!(&e.bytes[12..16], &0xBEEFu32.to_le_bytes());
        // Name descriptor: tag 0xFC, 13 chars max.
        assert_eq!(e.bytes[75], 0xFC);
        assert_eq!(&e.bytes[77..90], b"Steam Deck OL");
        // Serial descriptor: tag 0xFF.
        assert_eq!(e.bytes[93], 0xFF);
        assert_eq!(&e.bytes[95..107], b"LVGD0000BEEF");
    }

    #[test]
    fn empty_name_gets_default_and_non_ascii_is_sanitized() {
        let e = generate(&mode(1920, 1080, 60_000), &[0u16], 1);
        assert_eq!(&e.bytes[77..88], b"Luminal VGD");

        let e = generate(&mode(1920, 1080, 60_000), &utf16("Téléviseur"), 1);
        assert_eq!(&e.bytes[77..87], b"T?l?viseur");
    }

    #[test]
    fn deep_color_changes_input_byte() {
        let m10 = Mode::validate(1920, 1080, 60_000, 110, 1, caps::HDR10).unwrap();
        let e = generate(&m10, &utf16("X"), 1);
        assert_eq!(e.bytes[20], 0xB5);
        let m12 =
            Mode::validate(1920, 1080, 60_000, 112, 1, caps::HDR10 | caps::HDR12_BIT).unwrap();
        let e = generate(&m12, &utf16("X"), 1);
        assert_eq!(e.bytes[20], 0xC5);
    }
}

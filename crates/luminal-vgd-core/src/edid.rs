// SPDX-License-Identifier: AGPL-3.0-only
//! Per-monitor EDID generation: 128-byte base block + CTA-861 extension.
//!
//! SudoVDA shipped one static high-res EDID blob for every monitor; we
//! generate a fresh one per created monitor (FEATURE-MATRIX.md "High-res
//! EDID"), so the preferred timing, display name, product code, serial,
//! and physical dimensions all match the session exactly. Windows keys
//! per-display settings on (manufacturer, product, serial) — the identity
//! module derives those from `display_id`, which is what makes a returning
//! client "the same monitor".
//!
//! The CTA-861 extension (structure folded in from libvirtualdisplay, MIT
//! — see THIRD-PARTY-NOTICES.md) carries what the base block can't:
//! - HDR static metadata (PQ EOTF + ST 2086 luminance) — required for the
//!   Windows HDR toggle to appear reliably on the virtual display;
//! - BT.2020 colorimetry;
//! - physical dimensions also ride the base block (bytes 21/22 and the
//!   DTD), driving correct DPI scaling instead of "size unknown".
//!
//! Scope notes:
//! - A Detailed Timing Descriptor caps at 4095×4095 and 655.35 MHz pixel
//!   clock. Modes beyond that get a stand-in DTD; the IddCx mode list
//!   always carries the exact session modes regardless. The EDID is
//!   identity + hint, never the mode authority.

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

/// Defaults when the request passes 0 (≈27" 16:9, libvirtualdisplay's
/// defaults).
pub const DEFAULT_PHYSICAL_WIDTH_MM: u32 = 600;
pub const DEFAULT_PHYSICAL_HEIGHT_MM: u32 = 340;
/// EDID physical-size fields saturate at 2550 mm (255 cm).
pub const MAX_PHYSICAL_SIZE_MM: u32 = 2550;

/// CTA-861.3 luminance codes for the HDR static metadata block — the
/// DEFAULTS, used when the create request passes `max_nits = 0`.
/// max: L = 50·2^(code/32) → code 138 ≈ 993 nits.
/// maxFALL: code 96 ≈ 400 nits.
/// min: L = Lmax·(code/255)²/100 → code 18 ≈ 0.05 nits.
const HDR_MAX_LUMINANCE_CODE: u8 = 138;
const HDR_MAX_FALL_CODE: u8 = 96;
const HDR_MIN_LUMINANCE_CODE: u8 = 18;

/// The three CTA-861.3 luminance codes for a monitor's HDR block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HdrLuminanceCodes {
    max: u8,
    max_fall: u8,
    min: u8,
}

/// Inverse of the CTA-861.3 max-luminance coding L = 50·2^(code/32),
/// rounded to the nearest code. Code 0 means "no data", so the encodable
/// floor is code 1 ≈ 51.1 nits; requests below that clamp up.
fn max_luminance_code(nits: f64) -> u8 {
    let code = (32.0 * (nits / 50.0).log2()).round();
    code.clamp(1.0, 255.0) as u8
}

/// Decode a max-luminance code back to nits (for derived values).
fn decode_max_luminance(code: u8) -> f64 {
    50.0 * f64::powf(2.0, f64::from(code) / 32.0)
}

/// Compute the HDR block's luminance codes for a host-requested peak.
/// `max_nits == 0` keeps the historical defaults byte-for-byte (proto
/// 0.3 hosts and zero-initialized requests land here). Otherwise:
///  - max: nearest CTA code to the request (800 nits = code 128 exactly;
///    1000 nits rounds to code 138 = the 993-nit default);
///  - maxFALL: keeps the defaults' ~40% ratio of the decoded peak;
///  - min: stays at the defaults' absolute 0.05 nits, re-coded against
///    the new peak (min coding depends on Lmax).
fn hdr_luminance_codes(max_nits: u32) -> HdrLuminanceCodes {
    if max_nits == 0 {
        return HdrLuminanceCodes {
            max: HDR_MAX_LUMINANCE_CODE,
            max_fall: HDR_MAX_FALL_CODE,
            min: HDR_MIN_LUMINANCE_CODE,
        };
    }
    let max = max_luminance_code(f64::from(max_nits));
    let max_decoded = decode_max_luminance(max);
    let default_ratio = decode_max_luminance(HDR_MAX_FALL_CODE) / decode_max_luminance(HDR_MAX_LUMINANCE_CODE);
    let max_fall = max_luminance_code(max_decoded * default_ratio);
    // L = Lmax·(code/255)²/100 → code = 255·sqrt(100·Lmin/Lmax), Lmin fixed
    // at the default 0.05 nits.
    let min = (255.0 * (100.0 * 0.05 / max_decoded).sqrt()).round().clamp(0.0, 255.0) as u8;
    HdrLuminanceCodes { max, max_fall, min }
}

/// Which timing the DTD ended up describing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DtdTiming {
    /// DTD carries the exact preferred mode.
    Exact,
    /// Preferred mode exceeds DTD limits; DTD carries this stand-in
    /// (width, height, refresh_millihz).
    StandIn(u32, u32, u32),
}

pub struct Edid {
    /// Base block + CTA-861 extension.
    pub bytes: [u8; 256],
    pub dtd: DtdTiming,
}

/// Everything identity- or mode-bearing that lands in the EDID.
pub struct EdidParams<'a> {
    /// Preferred mode (`modes[0]` of the create request).
    pub mode: &'a Mode,
    /// NUL-padded UTF-16 friendly name.
    pub friendly_name: &'a [u16],
    /// From `identity::serial_from_display_id` (or the explicit override).
    pub serial: u32,
    /// From `identity::{temporary,permanent}_product_code`.
    pub product_code: u16,
    /// 0 => defaults.
    pub physical_width_mm: u32,
    pub physical_height_mm: u32,
    /// Desired HDR peak luminance in nits for the CTA-861.3 block.
    /// 0 => historical defaults (≈993 nits). Ignored for SDR modes.
    /// Values quantize to the CTA 8-bit log code (~2% steps) with an
    /// encodable floor of ≈51 nits. Proto 0.4 (`max_nits`).
    pub max_nits: u32,
}

/// Pixel clock in 10 kHz units under the fixed blanking model, rounded.
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

fn choose_dtd(mode: &Mode) -> DtdTiming {
    if dtd_fits(mode.width, mode.height, mode.refresh_millihz) {
        return DtdTiming::Exact;
    }
    // Keep the resolution if only the clock is the problem; else fall to 4K.
    let mut hz = mode.refresh_millihz / 2;
    while hz >= 23_000 && mode.width <= DTD_MAX_ACTIVE && mode.height <= DTD_MAX_ACTIVE {
        if dtd_fits(mode.width, mode.height, hz) {
            return DtdTiming::StandIn(mode.width, mode.height, hz);
        }
        hz /= 2;
    }
    DtdTiming::StandIn(3840, 2160, 60_000)
}

fn write_dtd(out: &mut [u8], width: u32, height: u32, refresh_millihz: u32, phys_w: u32, phys_h: u32) {
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
    out[12] = (phys_w & 0xFF) as u8;
    out[13] = (phys_h & 0xFF) as u8;
    out[14] = (((phys_w >> 8) as u8) << 4) | ((phys_h >> 8) as u8 & 0x0F);
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

fn checksum(block: &mut [u8]) {
    let sum: u32 = block[..block.len() - 1].iter().map(|&b| u32::from(b)).sum();
    block[block.len() - 1] = ((256 - (sum % 256)) % 256) as u8;
}

/// Generate the 256-byte EDID (base + CTA-861 extension).
pub fn generate(p: &EdidParams<'_>) -> Edid {
    let mode = p.mode;
    let phys_w = if p.physical_width_mm == 0 { DEFAULT_PHYSICAL_WIDTH_MM } else { p.physical_width_mm }
        .min(MAX_PHYSICAL_SIZE_MM);
    let phys_h = if p.physical_height_mm == 0 { DEFAULT_PHYSICAL_HEIGHT_MM } else { p.physical_height_mm }
        .min(MAX_PHYSICAL_SIZE_MM);

    let mut e = [0u8; 256];

    // ---- Base block -------------------------------------------------
    e[0..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
    // Manufacturer "NBF" (NortheBridge Foundation), big-endian 5-bit letters.
    e[8] = 0x38;
    e[9] = 0x46;
    // Product code (LE) — identity-derived: permanent vs temporary ranges.
    e[10] = (p.product_code & 0xFF) as u8;
    e[11] = (p.product_code >> 8) as u8;
    // Serial (LE) — identity-derived, keeps Windows settings per client.
    e[12..16].copy_from_slice(&p.serial.to_le_bytes());
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
    // Physical size, centimeters (saturating).
    e[21] = (phys_w / 10).min(255) as u8;
    e[22] = (phys_h / 10).min(255) as u8;
    e[23] = 120; // gamma 2.2
    e[24] = 0x02; // preferred timing includes native format
    // sRGB chromaticity.
    e[25..35].copy_from_slice(&[0xEE, 0x91, 0xA3, 0x54, 0x4C, 0x99, 0x26, 0x0F, 0x50, 0x54]);
    // No established timings; standard timings unused.
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
    write_dtd(&mut e[54..72], dw, dh, dhz, phys_w, phys_h);

    // Descriptor 2: display product name.
    let name: String = char::decode_utf16(p.friendly_name.iter().copied().take_while(|&c| c != 0))
        .map(|r| r.unwrap_or('?'))
        .collect();
    let name = if name.is_empty() { "Luminal VGD" } else { name.as_str() };
    write_text_descriptor(&mut e[72..90], 0xFC, name);

    // Descriptor 3: serial string (matches the binary serial).
    let serial_text = format!("LVGD{:08X}", p.serial);
    write_text_descriptor(&mut e[90..108], 0xFF, &serial_text);

    // Descriptor 4: dummy.
    e[108..126].fill(0);
    e[111] = 0x10;

    e[126] = 1; // one extension block: CTA-861
    checksum(&mut e[..128]);

    // ---- CTA-861 extension ------------------------------------------
    let (ext, hdr) = (&mut e[128..256], mode.hdr);
    ext[0] = 0x02; // CTA tag
    ext[1] = 0x03; // revision 3
    // ext[3]: underscan/audio/YCbCr flags + native DTD count — RGB-only
    // output, no audio, no native DTDs.
    ext[3] = 0x00;
    let mut i = 4;
    if hdr {
        // Colorimetry data block (extended tag 5): BT.2020 RGB + YCC.
        ext[i] = (7 << 5) | 3; // extended block, 3 payload bytes
        ext[i + 1] = 0x05;
        ext[i + 2] = 0xC0; // BT2020_RGB | BT2020_YCC
        ext[i + 3] = 0x00;
        i += 4;
        // HDR static metadata block (extended tag 6, CTA-861.3).
        let lum = hdr_luminance_codes(p.max_nits);
        ext[i] = (7 << 5) | 6;
        ext[i + 1] = 0x06;
        ext[i + 2] = 0x01 | 0x04; // EOTFs: traditional SDR + SMPTE ST 2084 (PQ)
        ext[i + 3] = 0x01; // static metadata descriptor type 1 (ST 2086)
        ext[i + 4] = lum.max;
        ext[i + 5] = lum.max_fall;
        ext[i + 6] = lum.min;
        i += 7;
    }
    ext[2] = i as u8; // DTDs would start here; none follow (zero padding)
    checksum(ext);

    Edid { bytes: e, dtd }
}

#[cfg(test)]
mod tests {
    use super::*;
    use luminal_driver_proto::caps;

    fn mode(w: u32, h: u32, hz: u32) -> Mode {
        Mode::validate(w, h, hz, 8, 0, caps::SDR10_BIT).unwrap()
    }

    fn hdr_mode() -> Mode {
        Mode::validate(1920, 1080, 60_000, 110, 1, caps::HDR10).unwrap()
    }

    fn utf16(s: &str) -> Vec<u16> {
        let mut v: Vec<u16> = s.encode_utf16().collect();
        v.push(0);
        v
    }

    fn params<'a>(m: &'a Mode, name: &'a [u16]) -> EdidParams<'a> {
        EdidParams {
            mode: m,
            friendly_name: name,
            serial: 42,
            product_code: 0x5001,
            physical_width_mm: 0,
            physical_height_mm: 0,
            max_nits: 0,
        }
    }

    #[test]
    fn nits_zero_keeps_default_luminance_codes() {
        let c = hdr_luminance_codes(0);
        assert_eq!(c, HdrLuminanceCodes { max: 138, max_fall: 96, min: 18 });
    }

    #[test]
    fn nits_800_encodes_exactly_code_128() {
        // 800 = 50·2^4 → code 128 with zero quantization error.
        let c = hdr_luminance_codes(800);
        assert_eq!(c.max, 128);
        // maxFALL keeps the defaults' ratio (400/993 of peak): 800·0.4027
        // = 322 nits → code 86.
        assert_eq!(c.max_fall, 86);
        // min stays 0.05 nits absolute: 255·sqrt(100·0.05/800) = 20.16 → 20.
        assert_eq!(c.min, 20);
    }

    #[test]
    fn nits_1000_rounds_to_the_historical_default_code() {
        // Code 138 (993 nits) is the nearest code to 1000 — the baked
        // pre-0.4 default was exactly "closest to 1000 nits".
        assert_eq!(hdr_luminance_codes(1000).max, 138);
    }

    #[test]
    fn nits_clamp_floor_and_monotonic() {
        // Below the CTA floor (code 1 ≈ 51.1 nits) clamps up; code 0 is
        // reserved for "no data" and never emitted.
        assert_eq!(hdr_luminance_codes(1).max, 1);
        assert_eq!(hdr_luminance_codes(40).max, 1);
        let mut last = 0u8;
        for nits in [51u32, 100, 200, 400, 800, 1000, 2000, 10_000] {
            let code = hdr_luminance_codes(nits).max;
            assert!(code >= last, "non-monotonic at {nits} nits");
            last = code;
        }
    }

    #[test]
    fn hdr_edid_carries_requested_nits_code() {
        let m = hdr_mode();
        let name = utf16("Nits");
        let mut p = params(&m, &name);
        p.max_nits = 800;
        let e = generate(&p);
        let ext = &e.bytes[128..256];
        assert_eq!(ext[12], 128, "max luminance code for 800 nits");
        assert_eq!(ext[13], 86, "maxFALL code");
        assert_eq!(ext[14], 20, "min luminance code");
        // Checksums must still balance after the substitution.
        assert_eq!(e.bytes[..128].iter().map(|&b| u32::from(b)).sum::<u32>() % 256, 0);
        assert_eq!(e.bytes[128..].iter().map(|&b| u32::from(b)).sum::<u32>() % 256, 0);
    }

    #[test]
    fn both_blocks_checksum_and_link() {
        let m = mode(1920, 1080, 60_000);
        let n = utf16("Living Room");
        let e = generate(&params(&m, &n));
        assert_eq!(&e.bytes[0..8], &[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        let base: u32 = e.bytes[..128].iter().map(|&b| u32::from(b)).sum();
        let ext: u32 = e.bytes[128..].iter().map(|&b| u32::from(b)).sum();
        assert_eq!(base % 256, 0, "base block checksum");
        assert_eq!(ext % 256, 0, "extension checksum");
        assert_eq!(e.bytes[126], 1, "one extension declared");
        assert_eq!(e.bytes[128], 0x02, "CTA-861 tag");
    }

    #[test]
    fn dtd_encodes_exact_1080p60_with_physical_size() {
        let m = mode(1920, 1080, 60_000);
        let n = utf16("X");
        let mut p = params(&m, &n);
        p.physical_width_mm = 700;
        p.physical_height_mm = 390;
        let e = generate(&p);
        assert_eq!(e.dtd, DtdTiming::Exact);
        let clock = u16::from_le_bytes([e.bytes[54], e.bytes[55]]);
        assert_eq!(clock, 14040); // (1920+160)(1080+45)·60 Hz
        // Physical size: base block cm + DTD mm.
        assert_eq!((e.bytes[21], e.bytes[22]), (70, 39));
        let dtd_w = u32::from(e.bytes[66]) | ((u32::from(e.bytes[68]) >> 4) << 8);
        let dtd_h = u32::from(e.bytes[67]) | ((u32::from(e.bytes[68]) & 0x0F) << 8);
        assert_eq!((dtd_w, dtd_h), (700, 390));
    }

    #[test]
    fn defaults_apply_when_physical_size_zero() {
        let m = mode(1920, 1080, 60_000);
        let n = utf16("X");
        let e = generate(&params(&m, &n));
        assert_eq!((e.bytes[21], e.bytes[22]), (60, 34)); // 600×340 mm
    }

    #[test]
    fn hdr_extension_carries_metadata_and_colorimetry() {
        let m = hdr_mode();
        let n = utf16("X");
        let e = generate(&params(&m, &n));
        let ext = &e.bytes[128..];
        // Colorimetry block.
        assert_eq!(ext[4], (7 << 5) | 3);
        assert_eq!(ext[5], 0x05);
        assert_eq!(ext[6], 0xC0, "BT.2020 RGB+YCC");
        // HDR static metadata block.
        assert_eq!(ext[8], (7 << 5) | 6);
        assert_eq!(ext[9], 0x06);
        assert_eq!(ext[10], 0x05, "SDR + PQ EOTFs");
        assert_eq!(ext[11], 0x01, "ST 2086 descriptor");
        assert_eq!(ext[12], 138);
        assert_eq!(ext[2], 15, "DTD offset after both blocks");
    }

    #[test]
    fn sdr_extension_is_empty_but_valid() {
        let m = mode(1920, 1080, 60_000);
        let n = utf16("X");
        let e = generate(&params(&m, &n));
        assert_eq!(e.bytes[128 + 2], 4, "no data blocks");
        assert!(e.bytes[132..255].iter().all(|&b| b == 0));
    }

    #[test]
    fn oversized_modes_get_stand_in_dtd() {
        let m = mode(7680, 4320, 60_000);
        let n = utf16("X");
        let e = generate(&params(&m, &n));
        assert_eq!(e.dtd, DtdTiming::StandIn(3840, 2160, 60_000));

        let m = mode(3840, 2160, 120_000);
        let e = generate(&params(&m, &n));
        assert_eq!(e.dtd, DtdTiming::StandIn(3840, 2160, 60_000));
    }

    #[test]
    fn identity_fields_are_stamped() {
        let m = mode(1920, 1080, 60_000);
        let n = utf16("Steam Deck OLED extra");
        let mut p = params(&m, &n);
        p.serial = 0xBEEF;
        p.product_code = 0x4002;
        let e = generate(&p);
        assert_eq!(&e.bytes[10..12], &[0x02, 0x40], "product code LE");
        assert_eq!(&e.bytes[12..16], &0xBEEFu32.to_le_bytes());
        assert_eq!(e.bytes[75], 0xFC);
        assert_eq!(&e.bytes[77..90], b"Steam Deck OL");
        assert_eq!(e.bytes[93], 0xFF);
        assert_eq!(&e.bytes[95..107], b"LVGD0000BEEF");
    }

    #[test]
    fn empty_name_gets_default_and_non_ascii_is_sanitized() {
        let m = mode(1920, 1080, 60_000);
        let e = generate(&params(&m, &[0u16]));
        assert_eq!(&e.bytes[77..88], b"Luminal VGD");

        let n = utf16("Téléviseur");
        let e = generate(&params(&m, &n));
        assert_eq!(&e.bytes[77..87], b"T?l?viseur");
    }

    #[test]
    fn deep_color_changes_input_byte() {
        let n = utf16("X");
        let e = generate(&params(&hdr_mode(), &n));
        assert_eq!(e.bytes[20], 0xB5);
        let m12 =
            Mode::validate(1920, 1080, 60_000, 112, 1, caps::HDR10 | caps::HDR12_BIT).unwrap();
        let e = generate(&params(&m12, &n));
        assert_eq!(e.bytes[20], 0xC5);
    }
}

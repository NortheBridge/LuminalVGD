// SPDX-License-Identifier: AGPL-3.0-only
//! Render-adapter selection, ported from SudoVDA's `gpuName` semantics:
//! the host may pin a specific adapter; when it doesn't, pick the adapter
//! with the most dedicated VRAM (SudoVDA's documented default). Explicit
//! selection is what makes hybrid-GPU laptops work — create the monitor on
//! the iGPU while the dGPU renders, or vice versa.

use crate::error::CoreError;

/// One enumerated render adapter, as reported by the shell (DXGI on
/// Windows; synthetic in tests).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterInfo {
    /// D3DKMT LUID packed as u64 (`(HighPart << 32) | LowPart`). Never 0
    /// for a real adapter — 0 is the wire value for "driver default".
    pub luid: u64,
    /// Dedicated VRAM in bytes.
    pub vram_bytes: u64,
    /// DXGI description string (diagnostics only; selection is by LUID).
    pub name: String,
    /// True for software rasterizers (WARP) — never auto-selected.
    pub software: bool,
}

/// Resolve the adapter for a monitor. `requested_luid == 0` means driver
/// default: the largest-VRAM hardware adapter. An explicit LUID must match
/// an enumerated hardware adapter exactly.
pub fn select_adapter(adapters: &[AdapterInfo], requested_luid: u64) -> Result<u64, CoreError> {
    if requested_luid != 0 {
        return adapters
            .iter()
            .find(|a| a.luid == requested_luid && !a.software)
            .map(|a| a.luid)
            .ok_or(CoreError::NoAdapter);
    }
    adapters
        .iter()
        .filter(|a| !a.software)
        .max_by(|a, b| {
            a.vram_bytes
                .cmp(&b.vram_bytes)
                // Deterministic tie-break so repeated creates agree.
                .then(b.luid.cmp(&a.luid))
        })
        .map(|a| a.luid)
        .ok_or(CoreError::NoAdapter)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(luid: u64, vram_gb: u64, name: &str) -> AdapterInfo {
        AdapterInfo {
            luid,
            vram_bytes: vram_gb << 30,
            name: name.into(),
            software: false,
        }
    }

    #[test]
    fn default_picks_largest_vram() {
        let adapters = [
            gpu(0x10, 8, "iGPU"),
            gpu(0x20, 16, "RTX 5080"),
            gpu(0x30, 4, "old dGPU"),
        ];
        assert_eq!(select_adapter(&adapters, 0), Ok(0x20));
    }

    #[test]
    fn explicit_luid_wins_even_if_smaller() {
        let adapters = [gpu(0x10, 8, "iGPU"), gpu(0x20, 16, "RTX 5080")];
        assert_eq!(select_adapter(&adapters, 0x10), Ok(0x10));
    }

    #[test]
    fn unknown_luid_is_an_error_not_a_fallback() {
        let adapters = [gpu(0x10, 8, "iGPU")];
        assert_eq!(select_adapter(&adapters, 0x99), Err(CoreError::NoAdapter));
    }

    #[test]
    fn software_adapters_never_selected() {
        let mut warp = gpu(0x40, 32, "WARP");
        warp.software = true;
        // Not by default…
        assert_eq!(select_adapter(&[warp.clone(), gpu(0x10, 2, "iGPU")], 0), Ok(0x10));
        // …and not explicitly either.
        assert_eq!(select_adapter(&[warp.clone()], 0x40), Err(CoreError::NoAdapter));
        // No hardware at all: error.
        assert_eq!(select_adapter(&[warp], 0), Err(CoreError::NoAdapter));
    }

    #[test]
    fn vram_tie_breaks_deterministically() {
        let a = [gpu(0x10, 8, "A"), gpu(0x20, 8, "B")];
        let b = [gpu(0x20, 8, "B"), gpu(0x10, 8, "A")];
        assert_eq!(select_adapter(&a, 0), select_adapter(&b, 0));
    }
}

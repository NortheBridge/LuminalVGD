// SPDX-License-Identifier: AGPL-3.0-only
//! The Windows IddCx shell (phase 2, CLAUDE.md plan).
//!
//! Everything the driver *decides* lives in [`crate::dispatch`] and
//! luminal-vgd-core; this module only moves bytes and owns OS handles.
//! Rules in force here (DESIGN.md §3.3): every wait is bounded, no IddCx
//! callback does D3D work inline, no lock is held across an IddCx call.
//!
//! There is exactly one LuminalVGD device (root-enumerated singleton, INF
//! hardware id `root\luminal_vgd`), so shell state is a process global
//! rather than WDF object context.

pub mod bindings;
mod control;
mod cursor;
mod dxgi;
mod entry;
mod monitors;
mod ring;
mod swapchain;

// Private to `shell` but visible to all submodules (descendants see
// ancestors' private items) — the macro does not accept a visibility.
tracelogging::define_provider!(PROVIDER, "NortheBridge.LuminalVGD");

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use crate::dispatch::{DeviceState, HandleCtx};
use luminal_vgd_core::modes::Mode;

/// Capabilities the shell actually delivers. HDR10/SDR10 landed with the
/// HDR tranche; HW_CURSOR = the cursor plane is republished into the
/// shared cursor section (the OS stops composing it into frames);
/// GAMMA_RAMP = the SetGammaRamp DDI is registered and acknowledged
/// (pixels stay pass-through — capture on physical displays is pre-LUT
/// too, so the stream parity is identical).
pub(crate) const SHELL_CAPS: u32 = luminal_driver_proto::caps::MULTI_MODE
    | luminal_driver_proto::caps::PERMANENT_POOL
    | luminal_driver_proto::caps::HDR10
    | luminal_driver_proto::caps::SDR10_BIT
    | luminal_driver_proto::caps::HW_CURSOR
    | luminal_driver_proto::caps::GAMMA_RAMP;

/// Monotonic build stamp reported in HANDSHAKE/GET_STATUS. Release
/// builds stamp it via the LUMINAL_VGD_BUILD environment variable
/// (scripts/package-release.ps1); the literal below is the dev fallback,
/// hand-bumped per signing round.
pub(crate) const DRIVER_BUILD: u32 = match option_env!("LUMINAL_VGD_BUILD") {
    Some(v) => {
        // const-context decimal parse (str::parse is not const).
        let bytes = v.as_bytes();
        let mut n: u32 = 0;
        let mut i = 0;
        while i < bytes.len() {
            assert!(bytes[i] >= b'0' && bytes[i] <= b'9', "LUMINAL_VGD_BUILD must be a decimal integer");
            n = n * 10 + (bytes[i] - b'0') as u32;
            i += 1;
        }
        n
    }
    None => 8,
};

/// NUL-terminated UTF-16 literal; size the array one past the text so the
/// terminator survives.
pub(crate) const fn wide<const N: usize>(s: &str) -> [u16; N] {
    let bytes = s.as_bytes();
    let mut out = [0u16; N];
    let mut i = 0;
    while i < bytes.len() && i < N {
        out[i] = bytes[i] as u16;
        i += 1;
    }
    out
}

/// Copyable wrapper for OS handles that are only used through APIs that
/// are themselves thread-safe (WDF/IddCx object handles, event handles).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct OsHandle(pub *mut core::ffi::c_void);
// SAFETY: see type docs — the wrapped value is an opaque kernel handle.
unsafe impl Send for OsHandle {}
unsafe impl Sync for OsHandle {}

pub(crate) struct MonitorRt {
    /// IDDCX_MONITOR object.
    pub monitor: OsHandle,
    /// The exact EDID served to the OS. Boxed so the pointer handed to
    /// IddCxMonitorCreate stays stable while the map rehashes.
    pub edid: Box<[u8; 256]>,
    pub modes: Vec<Mode>,
    pub worker: Option<swapchain::Worker>,
    /// The transport ring (section + policy + textures). Lives here, not
    /// in the worker, so sequences and the generation persist across
    /// swap-chain reassignments; the active worker drives it exclusively.
    pub ring: std::sync::Arc<Mutex<swapchain::FrameRing>>,
    /// Hardware-cursor worker + section (None when spawn failed — the OS
    /// then composes the cursor into frames, the pre-cursor behavior).
    /// The worker owns every cursor IddCx call, including the
    /// SetupHardwareCursor retry loop.
    pub cursor: Option<cursor::CursorRt>,
}

pub(crate) struct Shell {
    /// All driver decisions. Never held across an IddCx/WDF call that can
    /// re-enter the driver.
    pub dev: Mutex<DeviceState>,
    /// Per-open-handle handshake contexts, keyed by WDFFILEOBJECT.
    pub handles: Mutex<HashMap<usize, HandleCtx>>,
    /// session_id → live monitor runtime state.
    pub monitors: Mutex<HashMap<u64, MonitorRt>>,
    /// Set once EvtIddCxAdapterInitFinished succeeds; session IOCTLs are
    /// gated on this (control-plane requests can arrive first).
    pub adapter: OnceLock<OsHandle>,
    /// The WDF device, for registry persistence.
    pub wdf_device: OnceLock<OsHandle>,
    start: Instant,
}

static SHELL: OnceLock<Shell> = OnceLock::new();

impl Shell {
    pub fn init(dev: DeviceState) -> &'static Shell {
        SHELL.get_or_init(|| Shell {
            dev: Mutex::new(dev),
            handles: Mutex::new(HashMap::new()),
            monitors: Mutex::new(HashMap::new()),
            adapter: OnceLock::new(),
            wdf_device: OnceLock::new(),
            start: Instant::now(),
        })
    }

    pub fn get() -> &'static Shell {
        SHELL.get().expect("shell used before device add")
    }

    pub fn try_get() -> Option<&'static Shell> {
        SHELL.get()
    }

    /// Driver-clock milliseconds (same clock for leases and uptime).
    pub fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    pub fn ready(&self) -> bool {
        self.adapter.get().is_some()
    }
}

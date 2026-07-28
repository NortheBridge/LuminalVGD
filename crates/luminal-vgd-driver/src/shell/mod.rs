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
use std::sync::{mpsc, Mutex, OnceLock, TryLockError};
use std::time::{Duration, Instant};

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
    None => 14,
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
    /// Plug identity, kept so a TDR duck-out can re-arrive the same
    /// monitor (same container GUID via display_id, same connector)
    /// without a round trip through the host.
    pub display_id: u64,
    pub connector_index: u32,
    /// The adapter this monitor renders on: seeded from the plug's
    /// DeviceState choice, overwritten by every assign's
    /// RenderAdapterLuid (the authoritative value). The TDR recovery
    /// poller probes THIS adapter — probing the default adapter would
    /// false-positive off a healthy iGPU while the render dGPU is still
    /// wedged.
    pub adapter_luid: u64,
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

impl MonitorRt {
    /// Mark the transport ring DEAD (host unmaps) WITHOUT an unbounded
    /// wait: the frame worker pins the ring mutex for its whole lifetime,
    /// so after a deadline-detached (wedged) worker a plain lock() here
    /// would block forever — in unplug that wedges the effects worker
    /// (every later effect silently stalls), in final device exit a power
    /// callback. On deadline the mark is skipped (traced): the host's
    /// stale-heartbeat detection already treats a frozen ring as dead.
    pub(crate) fn mark_ring_dead(&self) {
        mark_ring_dead_arc(&self.ring);
    }
}

/// Bounded DEAD-marking for a bare ring Arc (see MonitorRt::mark_ring_dead
/// for the rationale) — also used for parked (ducked) monitors, which
/// hold their ring outside a MonitorRt.
pub(crate) fn mark_ring_dead_arc(ring: &std::sync::Arc<Mutex<swapchain::FrameRing>>) {
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        match ring.try_lock() {
            Ok(ring) => {
                if let Some(section) = &ring.section {
                    section.set_state(luminal_driver_proto::ring_state::DEAD);
                }
                return;
            }
            Err(TryLockError::Poisoned(poisoned)) => {
                let ring = poisoned.into_inner();
                if let Some(section) = &ring.section {
                    section.set_state(luminal_driver_proto::ring_state::DEAD);
                }
                return;
            }
            Err(TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    tracelogging::write_event!(
                        PROVIDER,
                        "RingDeadMarkTimeout",
                        level(Warning)
                    );
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// A monitor parked during a GPU reset: departed from the OS so TDR
/// recovery never waits on the indirect display, with everything needed
/// to re-arrive it kept alive (the ring Arc preserves sequences and the
/// generation across the gap, exactly like a swap-chain reassignment).
pub(crate) struct DuckedMonitor {
    pub session_id: u64,
    pub display_id: u64,
    pub connector_index: u32,
    pub adapter_luid: u64,
    pub edid: Box<[u8; 256]>,
    pub modes: Vec<Mode>,
    pub ring: std::sync::Arc<Mutex<swapchain::FrameRing>>,
}

pub(crate) struct Shell {
    /// All driver decisions. Never held across an IddCx/WDF call that can
    /// re-enter the driver.
    pub dev: Mutex<DeviceState>,
    /// Per-open-handle handshake contexts, keyed by WDFFILEOBJECT.
    pub handles: Mutex<HashMap<usize, HandleCtx>>,
    /// session_id → live monitor runtime state.
    pub monitors: Mutex<HashMap<u64, MonitorRt>>,
    /// Monitors departed during a GPU reset, awaiting re-arrival once the
    /// stack recovers (TDR duck-out; control.rs). Purged when the same
    /// session is destroyed or re-plugged while parked.
    pub ducked: Mutex<Vec<DuckedMonitor>>,
    /// One duck-out in flight at a time: set (CAS) when a frame worker
    /// observes device removal, cleared when the replug (or give-up)
    /// completes. Keeps N workers failing off one GPU reset from queueing
    /// N duck tasks.
    pub tdr_duck_pending: std::sync::atomic::AtomicBool,
    /// Duck cycles this adapter epoch. A wedge where device creation
    /// succeeds but activation still fails would otherwise flap
    /// duck→replug→duck forever; past the cap the give-up path runs
    /// instead. Reset by clear_adapter (a re-added device starts fresh).
    pub tdr_duck_cycles: std::sync::atomic::AtomicU32,
    /// Set once the deferred adapter bring-up completes (effects worker,
    /// after EvtIddCxAdapterInitFinished); session IOCTLs are gated on
    /// this (control-plane requests can arrive first). Cleared at final
    /// device exit so a replacement device re-arms bring-up. The epoch
    /// increments on every clear: a queued AdapterReady task captured
    /// under an older epoch must NOT republish a destroyed adapter, so
    /// publication is epoch-checked (set_adapter_if_epoch).
    adapter: Mutex<AdapterSlot>,
    /// The WDF device, for registry persistence. Overwritten on every
    /// device add — a re-added device supersedes its predecessor.
    wdf_device: Mutex<Option<OsHandle>>,
    /// Producer side of the effects worker (control::start_effects_thread)
    /// that owns every IddCx/D3D side effect. None only if the worker
    /// thread could not be spawned; queue_effects then applies inline.
    pub(crate) effects_tx: Mutex<Option<mpsc::Sender<control::EffectsTask>>>,
    start: Instant,
}

#[derive(Default)]
struct AdapterSlot {
    handle: Option<OsHandle>,
    epoch: u64,
}

static SHELL: OnceLock<Shell> = OnceLock::new();

impl Shell {
    pub fn init(dev: DeviceState) -> &'static Shell {
        SHELL.get_or_init(|| Shell {
            dev: Mutex::new(dev),
            handles: Mutex::new(HashMap::new()),
            monitors: Mutex::new(HashMap::new()),
            ducked: Mutex::new(Vec::new()),
            tdr_duck_pending: std::sync::atomic::AtomicBool::new(false),
            tdr_duck_cycles: std::sync::atomic::AtomicU32::new(0),
            adapter: Mutex::new(AdapterSlot::default()),
            wdf_device: Mutex::new(None),
            effects_tx: Mutex::new(control::start_effects_thread()),
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

    pub fn adapter(&self) -> Option<OsHandle> {
        self.adapter.lock().unwrap().handle
    }

    pub fn adapter_epoch(&self) -> u64 {
        self.adapter.lock().unwrap().epoch
    }

    /// Publish the adapter only if no clear_adapter intervened since
    /// `epoch` was captured — a stale (pre-teardown) AdapterReady task
    /// must never republish a destroyed adapter. Returns false when stale.
    pub fn set_adapter_if_epoch(&self, adapter: OsHandle, epoch: u64) -> bool {
        let mut slot = self.adapter.lock().unwrap();
        if slot.epoch != epoch {
            return false;
        }
        slot.handle = Some(adapter);
        true
    }

    pub fn clear_adapter(&self) {
        let mut slot = self.adapter.lock().unwrap();
        slot.handle = None;
        slot.epoch += 1;
        self.tdr_duck_cycles.store(0, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn wdf_device(&self) -> Option<OsHandle> {
        *self.wdf_device.lock().unwrap()
    }

    pub fn set_wdf_device(&self, device: OsHandle) {
        *self.wdf_device.lock().unwrap() = Some(device);
    }

    pub fn clear_wdf_device(&self) {
        *self.wdf_device.lock().unwrap() = None;
    }

    pub fn ready(&self) -> bool {
        self.adapter().is_some()
    }
}

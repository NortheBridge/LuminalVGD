// SPDX-License-Identifier: AGPL-3.0-only
//! Swap-chain assignment and the per-monitor frame worker: the phase-4
//! transport (DESIGN.md §3.1).
//!
//! Each acquired frame is GPU-copied into one of N named keyed-mutex
//! shared textures and published through the shared ring section. Every
//! slot decision comes from `core::ring::RingPolicy`; this file only
//! executes them. §3.3 discipline throughout: every wait is bounded
//! (frame waits and mutex acquires at 100 ms), a timeout drops the frame
//! and counts it, device loss rebuilds device + textures with a
//! generation bump while the monitor stays attached, and the header
//! heartbeat advances even when the desktop is idle.
//!
//! Keyed-mutex protocol note (phase-4 scope): the mutex serializes pixel
//! access — key 0 until a slot's first publish, key 1 thereafter (the
//! host releases back to 1). Readability is carried by the shared
//! `SlotMetadata.state`, not the key. Reader-side state reconciliation
//! (host marking slots FREE) lands with the phase-5 consumer.

use core::mem::zeroed;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use wdk_sys::{NTSTATUS, STATUS_PENDING, STATUS_SUCCESS};
use windows::core::Interface;
use windows::Win32::Foundation::{HANDLE, HMODULE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIDevice, IDXGIFactory4, IDXGIResource,
};
use windows::Win32::System::Threading::WaitForSingleObject;

use super::bindings::{self, ffi};
use super::ring::{
    acquire_mutex, create_shared_textures, qpc_now, AcquireOutcome, RingSection, SharedTexture,
};
use super::{OsHandle, Shell, PROVIDER};
use luminal_driver_proto::{ring_state, KMTX_ACQUIRE_TIMEOUT_MS};
use luminal_vgd_core::ring::RingPolicy;

/// Per-monitor ring state. Lives in `MonitorRt` so sequences and the
/// generation persist across swap-chain reassignments (mode changes,
/// adapter moves); the worker of the moment drives it exclusively.
pub(crate) struct FrameRing {
    pub policy: RingPolicy,
    /// None if section creation failed at plug time (transport disabled;
    /// the host falls back to WGC).
    pub section: Option<RingSection>,
    textures: Vec<SharedTexture>,
    tex_width: u32,
    tex_height: u32,
    /// DXGI_FORMAT raw value of the current textures; the OS switches the
    /// swapchain surface format when advanced color toggles (BGRA8 ⇄ FP16),
    /// which retires textures exactly like a size change.
    tex_format: u32,
    /// Per-slot: has this slot ever been published this generation? A
    /// never-published slot's keyed mutex is still at its creation key
    /// (0, driver); after the first publish it lives at key 1 (host).
    ever_published: Vec<bool>,
    /// True once any assign has run — later assigns bump the generation
    /// (new device ⇒ new textures ⇒ new names).
    assigned_before: bool,
}

/// A monitor's ring: the mutex-guarded [`FrameRing`] plus the LOCK-FREE
/// [`crate::tdr::RingLive`] mirror beside it.
///
/// The two halves are separate on purpose. `frame_loop` takes the
/// `FrameRing` mutex once and holds the guard for the entire life of the
/// worker (see the comment at the lock site below) — a load-bearing
/// invariant that `mark_ring_dead_arc` and `duck_all` are both written
/// around. Anything that must be readable WHILE a worker runs therefore
/// cannot live behind that mutex, which is exactly the mistake the first
/// cut of the TDR device-duck poller made: it `try_lock`ed to read the
/// ring state, always got `WouldBlock` from a healthy worker, and so could
/// never observe a recovery. `live` is the answer — published by the
/// worker at its two transitions, read by the poller with no lock at all.
pub(crate) struct RingHandle {
    pub live: crate::tdr::RingLive,
    pub ring: Mutex<FrameRing>,
}

impl RingHandle {
    pub fn new(ring: FrameRing) -> Self {
        // A ring whose section failed to create has no transport to
        // restore; the mirror records that once, at construction.
        let transport = ring.section.is_some();
        Self {
            live: crate::tdr::RingLive::new(transport),
            ring: Mutex::new(ring),
        }
    }
}

impl FrameRing {
    pub fn new(session_id: u64, ring_slots: u32) -> Self {
        let section = match RingSection::create(session_id, ring_slots) {
            Ok(s) => Some(s),
            Err(e) => {
                let code = e.code().0;
                tracelogging::write_event!(
                    PROVIDER,
                    "RingSectionCreateFailed",
                    level(Error),
                    u64("session", &session_id),
                    i32("hresult", &code)
                );
                None
            }
        };
        let policy = RingPolicy::new(ring_slots);
        let slots = policy.slot_count();
        Self {
            policy,
            section,
            textures: Vec::new(),
            tex_width: 0,
            tex_height: 0,
            tex_format: 0,
            ever_published: vec![false; slots],
            assigned_before: false,
        }
    }

    /// Retire the current textures and bump the generation (reassign,
    /// mode-size change, device loss). Sequences continue.
    fn retire_textures(&mut self) -> u32 {
        self.textures.clear();
        self.tex_width = 0;
        self.tex_height = 0;
        self.tex_format = 0;
        self.ever_published.iter_mut().for_each(|b| *b = false);
        let generation = self.policy.rebuild();
        if let Some(s) = &self.section {
            s.reset_slots();
            s.set_generation(generation);
        }
        generation
    }
}

pub(crate) struct Worker {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

/// How long stop() waits for the worker before detaching it — the same
/// §3.3 teardown budget as the cursor worker (cursor.rs). The frame
/// loop's waits are all ≤100 ms bounded, so a healthy worker exits well
/// inside the deadline; a worker wedged inside an OS call must never
/// extend unassign/teardown unboundedly.
const STOP_DEADLINE: Duration = Duration::from_millis(500);

impl Worker {
    /// Deadline-bounded stop: the thread re-checks the flag at least
    /// every 100 ms. On deadline (an OS call wedged) the thread is
    /// detached — it exits on its own if it ever unblocks (the stop flag
    /// stays set, and the ring Arc keeps its state alive).
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let Some(join) = self.join.take() else { return };
        let deadline = Instant::now() + STOP_DEADLINE;
        while !join.is_finished() {
            if Instant::now() >= deadline {
                tracelogging::write_event!(PROVIDER, "FrameWorkerStopTimeout", level(Warning));
                drop(join); // detach
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = join.join();
    }
}

pub unsafe extern "C" fn evt_assign(
    monitor: ffi::IDDCX_MONITOR,
    in_args: *const ffi::IDARG_IN_SETSWAPCHAIN,
) -> NTSTATUS {
    let inp = &*in_args;
    let swapchain = OsHandle(inp.hSwapChain.cast());
    let frame_event = OsHandle(inp.hNextSurfaceAvailable.cast());
    let luid = ((inp.RenderAdapterLuid.HighPart as u32 as u64) << 32)
        | inp.RenderAdapterLuid.LowPart as u64;

    let shell = Shell::get();
    let stop = Arc::new(AtomicBool::new(false));
    // Take what the spawn needs under the lock — and install the new
    // worker's PLACEHOLDER (stop flag, no join yet) in the same atomic
    // step that removes the old worker: an unassign/unplug landing while
    // the old worker's (possibly 500 ms) stop runs below takes the
    // placeholder and sets its flag, which the about-to-spawn thread
    // honors at its first re-check — no window where teardown finds
    // nothing to stop. The old worker itself stops OUTSIDE the lock:
    // stop() can wait its full deadline, and no such wait may run under
    // a lock other IddCx callbacks also take (§3.3).
    let (session_id, ring, old) = {
        let mut monitors = shell.monitors.lock().unwrap();
        let monitor_count = monitors.len() as u32;
        let Some((&session_id, rt)) = monitors
            .iter_mut()
            .find(|(_, rt)| rt.monitor == OsHandle(monitor.cast()))
        else {
            tracelogging::write_event!(
                PROVIDER,
                "AssignSwapChainUnknownMonitor",
                level(Error),
                u64("monitor_ptr", &(monitor as u64)),
                u32("known_monitors", &monitor_count)
            );
            return STATUS_SUCCESS;
        };
        tracelogging::write_event!(
            PROVIDER,
            "AssignSwapChain",
            level(Informational),
            u64("session", &session_id),
            u64("luid", &luid)
        );
        // The assign's RenderAdapterLuid is the authoritative render
        // adapter for this monitor; the TDR recovery poller probes it.
        rt.adapter_luid = luid;
        let old = rt.worker.replace(Worker { stop: stop.clone(), join: None });
        (session_id, rt.ring.clone(), old)
    };
    if let Some(old) = old {
        old.stop();
    }

    let stop_thread = stop.clone();
    let join = std::thread::spawn(move || {
        frame_loop(session_id, swapchain, frame_event, luid, ring, stop_thread)
    });
    // Adopt the join handle into the placeholder — matched by session_id,
    // monitor identity, AND the same stop flag (a re-plug reusing the
    // session id or monitor pointer must not adopt a stranger's thread).
    let mut join = Some(join);
    {
        let mut monitors = shell.monitors.lock().unwrap();
        if let Some(rt) = monitors.get_mut(&session_id) {
            if rt.monitor == OsHandle(monitor.cast()) {
                if let Some(worker) = rt.worker.as_mut() {
                    if Arc::ptr_eq(&worker.stop, &stop) {
                        worker.join = join.take();
                    }
                }
            }
        }
    }
    if let Some(join) = join {
        // Teardown (or a replacement assign) consumed the placeholder in
        // the window: this thread must not run against the swapchain —
        // stop it (bounded).
        tracelogging::write_event!(
            PROVIDER,
            "AssignRacedUnplug",
            level(Warning),
            u64("session", &session_id)
        );
        Worker { stop, join: Some(join) }.stop();
    }
    // NOTE: nothing here may call back into IddCx. This callback is a
    // win32k callout; an IddCx call from inside it can deadlock against
    // the locks win32k holds while calling us (callout-watchdog 0x1b8
    // storms → system hang, observed 2026-07-23). The cursor worker owns
    // all cursor IddCx calls on its own thread.
    STATUS_SUCCESS
}

pub unsafe extern "C" fn evt_unassign(monitor: ffi::IDDCX_MONITOR) -> NTSTATUS {
    tracelogging::write_event!(PROVIDER, "UnassignSwapChain", level(Informational));
    let shell = Shell::get();
    let worker = {
        let mut monitors = shell.monitors.lock().unwrap();
        monitors
            .values_mut()
            .find(|rt| rt.monitor == OsHandle(monitor.cast()))
            .and_then(|rt| rt.worker.take())
    };
    if let Some(worker) = worker {
        worker.stop();
    }
    STATUS_SUCCESS
}

/// Can the given render adapter create a D3D11 device right now? The
/// TDR recovery poller's "is the display stack back" probe. Probing the
/// SPECIFIC render LUID matters: on a hybrid iGPU+dGPU machine a
/// default-adapter probe would answer for a healthy iGPU while the
/// wedged dGPU our monitors render on is still down.
///
/// WARNING: D3D11CreateDevice against a wedged stack can block
/// indefinitely — callers must run this on a throwaway thread with a
/// deadline, never on the poller's own clock (the build-12 lesson:
/// individually-bounded waits compose into unbounded ones).
pub(crate) fn probe_adapter_on_luid(luid: u64) -> bool {
    create_device_on_luid(luid).is_ok()
}

fn create_device_on_luid(
    luid: u64,
) -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext, IDXGIDevice)> {
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory1()?;
        let adapter_luid = windows::Win32::Foundation::LUID {
            LowPart: luid as u32,
            HighPart: (luid >> 32) as i32,
        };
        let adapter: IDXGIAdapter = factory.EnumAdapterByLuid(adapter_luid)?;
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
        let device = device.unwrap();
        let context = context.unwrap();
        let dxgi = device.cast::<IDXGIDevice>()?;
        Ok((device, context, dxgi))
    }
}

/// How often the header heartbeat must advance even with no frames.
const HEARTBEAT_EVERY: Duration = Duration::from_millis(250);

/// IddCx returns COM-style E_PENDING (0x8000000A) from
/// ReleaseAndAcquireBuffer when no frame is ready — "wait on the event",
/// not an error, despite being a FAILED-severity HRESULT. (Kernel-style
/// STATUS_PENDING is accepted too, for belt and braces.)
const E_PENDING: NTSTATUS = 0x8000_000Au32 as NTSTATUS;

/// Publishes the frame worker's exit to the lock-free mirror on EVERY way
/// out of `frame_loop` after it went live — the stop re-checks scattered
/// through the loop, the failure returns, and an unwind. A missed exit
/// would leave a corpse advertising itself as publishing, which is what
/// the requalify guard refuses to depart.
struct LiveMark<'a>(&'a crate::tdr::RingLive);

impl Drop for LiveMark<'_> {
    fn drop(&mut self) {
        self.0.publish_worker_exit();
    }
}

fn frame_loop(
    session_id: u64,
    swapchain: OsHandle,
    frame_event: OsHandle,
    luid: u64,
    handle: Arc<RingHandle>,
    stop: Arc<AtomicBool>,
) {
    tracelogging::write_event!(
        PROVIDER,
        "FrameWorkerSpawned",
        level(Informational),
        u64("session", &session_id)
    );
    // Create the D3D device BEFORE touching the ring mutex, and hand it to
    // the OS BEFORE any ring bookkeeping. The create can take seconds on a
    // cold GPU stack, and holding the ring across it is what killed build
    // 12's cold-boot activation: the OS's routine ~10 ms unassign deadline-
    // detached this worker with the mutex still pinned, and the replacement
    // assign's worker blocked on ring.lock() before it could ever call
    // IddCxSwapChainSetDevice — the modeset transaction starved and rolled
    // the path back. SetDevice needs no ring state, so nothing on the
    // activation-critical path may wait on the ring.
    let d3d = match create_device_on_luid(luid) {
        Ok(d) => d,
        Err(e) => {
            let code = e.code().0;
            tracelogging::write_event!(
                PROVIDER,
                "SwapChainDeviceCreateFailed",
                level(Error),
                u64("session", &session_id),
                i32("hresult", &code)
            );
            // BUILD-16 RE-ARM. Device creation failing is how a GPU death
            // presents when it happens between an unassign and the next
            // assign: there is no device yet, so `maybe_queue_tdr_duck`
            // (which interrogates one with GetDeviceRemovedReason) can
            // never be reached from here, and before build 16 this site
            // armed nothing at all. In device-duck mode that would leave
            // the ring unwatched — heartbeat frozen, and the bounded
            // 10-minute deadline arm never reachable.
            //
            // The REBUILDING mark is load-bearing, not cosmetic: the
            // recovery poller decides "did the transport come back" from
            // the ring state, so a duck armed here against a still-ACTIVE
            // ring would settle on its first tick and do nothing.
            //
            // Re-arming is idempotent: `queue_tdr_duck`'s one-in-flight CAS
            // collapses a storm of failing workers into a single duck, and
            // while a poller is already watching this is a cheap no-op. The
            // cadence is the OS's reassign rate, not a spin.
            //
            // Gated to device-duck mode on purpose: builds 14/15 never
            // ducked from this site, and the legacy gate exists to restore
            // their behaviour faithfully, not an improved version of it.
            // The stop check keeps a routine teardown (the OS's ~10 ms
            // unassign landing inside a slow create) from reading as a GPU
            // reset.
            if !stop.load(Ordering::SeqCst)
                && Shell::get().tdr_duck_mode() == crate::dispatch::TDR_DUCK_DEVICE
            {
                super::mark_ring_rebuilding_arc(&handle);
                // The HRESULT travels WITH the arm (build 16 shipped it
                // only on SwapChainDeviceCreateFailed, so a capture that
                // wrapped past that event could not say what the duck was
                // armed against). Every arm site now names its cause.
                super::control::queue_tdr_duck(session_id, code);
            }
            return;
        }
    };
    // The OS routinely unassigns ~10 ms after assign; if that (or a
    // detach) already stopped us during device creation, the swapchain
    // may be gone — never SetDevice a stale handle.
    if stop.load(Ordering::SeqCst) {
        return;
    }

    unsafe {
        let mut set_dev: ffi::IDARG_IN_SWAPCHAINSETDEVICE = zeroed();
        set_dev.pDevice = d3d.2.as_raw().cast();
        let status = bindings::swapchain_set_device(swapchain.0.cast(), &set_dev);
        if status != STATUS_SUCCESS {
            tracelogging::write_event!(
                PROVIDER,
                "SwapChainSetDeviceFailed",
                level(Error),
                i32("status", &status)
            );
            return;
        }
    }

    // THE RING MUTEX IS PINNED FROM HERE TO THE END OF THIS FUNCTION.
    // The `let ring = &mut *guard` below is a reborrow, not a drop — the
    // guard lives until the worker returns. That is deliberate (the worker
    // owns the ring exclusively while it runs) and load-bearing elsewhere:
    // `mark_ring_dead_arc` is a bounded try_lock poll BECAUSE of it. The
    // consequence for anything outside this thread is absolute — no
    // FrameRing field can be read while a worker is alive — which is why
    // the TDR recovery signal lives in `handle.live`, beside the mutex
    // rather than behind it.
    //
    // Poison recovery matches mark_ring_dead: a prior worker's panic must
    // not cascade a second panic into WUDFHost on the next assign.
    let mut guard = handle.ring.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // Re-check after the (possibly long) lock wait: a worker stopped while
    // queued behind a wedged predecessor may have been deadline-detached —
    // its swapchain is then already torn down and must not be touched
    // (mirrors the cursor worker's post-wait re-check). A wait here is
    // harmless to activation: the OS already has its device.
    if stop.load(Ordering::SeqCst) {
        return;
    }
    let ring = &mut *guard;

    // A reassignment means a new device: retire the old textures and bump
    // the generation so the host re-opens by name. Runs before any publish
    // of this generation, which is all the ordering the host protocol
    // needs — SetDevice having happened first only means acquirable frames
    // queue in the swapchain until this loop starts consuming them.
    if ring.assigned_before {
        ring.retire_textures();
    }
    ring.assigned_before = true;

    if let Some(s) = &ring.section {
        s.set_state(ring_state::ACTIVE);
    }
    // PUBLISH: the transport is live. SetDevice has succeeded on a freshly
    // created device, the textures are retired and the shared section is
    // ACTIVE — this is the exact instant a TDR device-duck poller is
    // waiting to observe, and `live` is the only way it CAN observe it
    // (the section it mirrors is now behind a mutex this thread owns for
    // the rest of its life). Deliberately not published earlier, right
    // after IddCxSwapChainSetDevice: a worker that then blocked on the
    // ring lock behind a detached corpse would be advertising a transport
    // that never actually starts, and "recovered" concluded from a ring
    // nobody is driving is the one mistake that strands the display.
    //
    // `_live` is RAII so EVERY exit below — the stop re-checks, the
    // failure returns, an unwind — publishes the matching worker-exit.
    let live_generation = handle.live.publish_worker_live();
    let _live = LiveMark(&handle.live);
    tracelogging::write_event!(
        PROVIDER,
        "FrameLoopStart",
        level(Informational),
        u64("session", &session_id),
        u32("generation", &ring.policy.generation),
        u64("live_generation", &live_generation)
    );

    let mut last_heartbeat = Instant::now();
    while !stop.load(Ordering::SeqCst) {
        if last_heartbeat.elapsed() >= HEARTBEAT_EVERY {
            if let Some(s) = &ring.section {
                s.heartbeat();
            }
            last_heartbeat = Instant::now();
        }

        // Buffer2 is mandatory for CAN_PROCESS_FP16 adapters (IddCx 1.10):
        // METADATA2 carries per-frame HDR metadata / surface color space /
        // SDR white level alongside the same surface + QPC fields.
        let mut in_args: ffi::IDARG_IN_RELEASEANDACQUIREBUFFER2 = unsafe { zeroed() };
        in_args.Size = size_of::<ffi::IDARG_IN_RELEASEANDACQUIREBUFFER2>() as u32;
        let mut out: ffi::IDARG_OUT_RELEASEANDACQUIREBUFFER2 = unsafe { zeroed() };
        out.MetaData.Size = size_of::<ffi::IDDCX_METADATA2>() as u32;
        let status = unsafe {
            bindings::swapchain_release_and_acquire_buffer2(swapchain.0.cast(), &mut in_args, &mut out)
        };
        // Stopping: unassign/teardown may already have returned (deadline
        // detach) — no further swapchain or surface access. Skipping the
        // final FinishedProcessingFrame is safe; the OS reclaims abandoned
        // swap-chain buffers at teardown.
        if stop.load(Ordering::SeqCst) {
            return;
        }
        if status == STATUS_PENDING || status == E_PENDING {
            unsafe {
                let _ = WaitForSingleObject(HANDLE(frame_event.0), 100);
            }
            continue;
        }
        if status != STATUS_SUCCESS {
            // The swap chain is gone (device loss, or the OS is swapping
            // in a replacement — routine right after activation). Do NOT
            // retry against this swapchain: mark REBUILDING, retire the
            // textures (generation bump), and exit so the OS's
            // unassign/assign cycle drives recovery with a fresh worker.
            // Holding the dead swapchain in a retry loop stalls the
            // compositor's teardown and gets the host terminated.
            tracelogging::write_event!(
                PROVIDER,
                "AcquireBufferFailedExit",
                level(Warning),
                u64("session", &session_id),
                i32("status", &status)
            );
            if let Some(s) = &ring.section {
                s.set_state(ring_state::REBUILDING);
            }
            ring.retire_textures();
            // Mirror the state and drop the live flag BEFORE arming the
            // duck: the poller's very first tick must see a ring that is
            // REBUILDING and unowned, never a corpse still advertising
            // itself as publishing.
            handle.live.publish_state(ring_state::REBUILDING);
            handle.live.publish_worker_exit();
            maybe_queue_tdr_duck(session_id, &d3d.0);
            return;
        }

        // Frame in hand. pSurface stays valid until the next acquire.
        let meta = &out.MetaData;
        if meta.pSurface.is_null() {
            unsafe { bindings::swapchain_finished_processing_frame(swapchain.0.cast()) };
            continue;
        }
        let publish_result = publish_frame(session_id, ring, &d3d.0, &d3d.1, meta, &stop);
        // publish_frame contains TDR-scale D3D wedge points (texture
        // creation, CopyResource); if the stop deadline detached us while
        // inside one, the swapchain is already torn down.
        if stop.load(Ordering::SeqCst) {
            return;
        }
        unsafe { bindings::swapchain_finished_processing_frame(swapchain.0.cast()) };
        if let Some(s) = &ring.section {
            s.heartbeat();
        }
        last_heartbeat = Instant::now();

        if let Err(e) = publish_result {
            // D3D failure mid-publish (device removed and friends): same
            // exit-and-let-the-OS-reassign policy as acquire failures.
            let code = e.code().0;
            tracelogging::write_event!(
                PROVIDER,
                "PublishFrameErrorExit",
                level(Warning),
                u64("session", &session_id),
                i32("hresult", &code)
            );
            if let Some(s) = &ring.section {
                s.set_state(ring_state::REBUILDING);
            }
            ring.retire_textures();
            // Same ordering as the acquire-failure arm above.
            handle.live.publish_state(ring_state::REBUILDING);
            handle.live.publish_worker_exit();
            maybe_queue_tdr_duck(session_id, &d3d.0);
            return;
        }
    }
}

/// Distinguish "this swapchain is being replaced" (routine — the OS's
/// ~10 ms unassign, mode switches) from "the GPU itself reset" and queue
/// a TDR duck only for the latter. The discriminator is the D3D device:
/// swapchain-level failures with a healthy device are handled by the OS's
/// unassign→assign cycle; a removed device means every monitor on this
/// adapter is about to fail the same way.
///
/// By the time this runs, the caller has ALREADY done everything build
/// 16's "duck the device" means: the ring is REBUILDING, the textures are
/// retired with a generation bump, the swapchain is abandoned, and `d3d`
/// (the ID3D11Device/Context/IDXGIDevice triple) drops at the caller's
/// `return`. What `control::queue_tdr_duck` then decides is only whether
/// the DISPLAY goes too — see the build-16 gate there.
fn maybe_queue_tdr_duck(session_id: u64, device: &ID3D11Device) {
    let removed = unsafe { device.GetDeviceRemovedReason() };
    if let Err(reason) = removed {
        let code = reason.code().0;
        tracelogging::write_event!(
            PROVIDER,
            "TdrDeviceRemoved",
            level(Warning),
            u64("session", &session_id),
            i32("reason", &code)
        );
        // Carry the removal reason into the arm so the duck's own events
        // name what caused it — a capture that has wrapped past
        // TdrDeviceRemoved must still be able to say why a display stopped
        // publishing.
        super::control::queue_tdr_duck(session_id, code);
    }
}

/// Copy the acquired frame into a ring slot and publish it. Any error is
/// returned after the ring bookkeeping is made consistent (abort/drop).
fn publish_frame(
    session_id: u64,
    ring: &mut FrameRing,
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    meta: &ffi::IDDCX_METADATA2,
    stop: &AtomicBool,
) -> windows::core::Result<()> {
    if ring.section.is_none() {
        return Ok(()); // transport disabled; drain-only
    }

    let raw_surface: *mut core::ffi::c_void = meta.pSurface.cast();
    let frame_tex: ID3D11Texture2D = unsafe {
        match IDXGIResource::from_raw_borrowed(&raw_surface) {
            Some(f) => f.cast()?,
            None => return Ok(()),
        }
    };
    let mut desc = unsafe { zeroed() };
    unsafe { frame_tex.GetDesc(&mut desc) };

    // Lazy texture (re)creation: first frame, or the committed mode
    // changed size, or the surface format changed (advanced-color toggle
    // switches BGRA8 ⇄ FP16). Any change is a full generation bump so the
    // host re-opens textures by name and re-latches the format.
    if ring.textures.is_empty()
        || desc.Width != ring.tex_width
        || desc.Height != ring.tex_height
        || desc.Format.0 as u32 != ring.tex_format
    {
        if !ring.textures.is_empty() {
            ring.retire_textures();
        }
        let slots = ring.policy.slot_count() as u32;
        ring.textures = create_shared_textures(
            device,
            session_id,
            ring.policy.generation,
            slots,
            desc.Width,
            desc.Height,
            desc.Format,
        )?;
        ring.tex_width = desc.Width;
        ring.tex_height = desc.Height;
        ring.tex_format = desc.Format.0 as u32;
        tracelogging::write_event!(
            PROVIDER,
            "RingTexturesCreated",
            level(Informational),
            u64("session", &session_id),
            u32("generation", &ring.policy.generation),
            u32("width", &desc.Width),
            u32("height", &desc.Height),
            u32("format", &(desc.Format.0 as u32))
        );
    }

    // Absorb the host's reader transitions (READING claims, FREE
    // releases) so writer decisions respect checked-out slots and reuse
    // consumed ones without counting drops.
    if let Some(s) = &ring.section {
        for index in 0..ring.policy.slot_count() {
            ring.policy.reconcile_shared(index, s.slot_state(index));
        }
    }

    let Some(writer) = ring.policy.writer_acquire() else {
        // Reader holds everything (pathological): drop, never block.
        if let Some(s) = &ring.section {
            s.heartbeat();
        }
        return Ok(());
    };

    // Take the slot ATOMICALLY in shared memory before touching it. The
    // policy's choice above is based on the reconcile snapshot; the host's
    // claim CAS (PUBLISHED → READING) can land in between. CASing
    // `expected → WRITING` on the same atomic serializes the two takers —
    // without this, an overwrite could relabel/clobber a slot mid-claim
    // (the ring-stall / mislabeled-frame race). On loss: drop this frame
    // and let the next reconcile absorb the host's READING.
    if let Some(s) = &ring.section {
        let expected = if writer.overwrote.is_some() {
            luminal_driver_proto::slot_state::PUBLISHED
        } else {
            luminal_driver_proto::slot_state::FREE
        };
        if !s.try_take_slot_writing(writer.index, expected) {
            ring.policy.writer_abort(writer.index);
            s.heartbeat();
            return Ok(());
        }
    }
    let slot: &SharedTexture = &ring.textures[writer.index];

    // Key 0 until the slot's first publish this generation; key 1
    // forever after (module docs — readability travels in
    // SlotMetadata.state, the mutex only guards pixel access).
    let key = if ring.ever_published[writer.index] {
        luminal_driver_proto::KMTX_KEY_HOST
    } else {
        luminal_driver_proto::KMTX_KEY_DRIVER
    };

    match acquire_mutex(&slot.mutex, key, KMTX_ACQUIRE_TIMEOUT_MS) {
        AcquireOutcome::Acquired => {}
        AcquireOutcome::TimedOut => {
            // Bounded wait expired (host holding the pixels too long):
            // drop this frame, never block the compositor path.
            ring.policy.writer_abort(writer.index);
            if let Some(s) = &ring.section {
                s.reset_slot_free(writer.index);
            }
            return Ok(());
        }
        AcquireOutcome::DeviceLost(hr) => {
            ring.policy.writer_abort(writer.index);
            if let Some(s) = &ring.section {
                // The take-CAS put the shared state at WRITING; release it
                // so the slot isn't leaked unwritable if the ring survives.
                s.reset_slot_free(writer.index);
            }
            return Err(windows::core::Error::from_hresult(hr));
        }
    }

    // Stopping (possibly deadline-detached mid-wedge in texture creation
    // or the keyed-mutex wait): frame_tex belongs to the swapchain, which
    // teardown may already have destroyed — abort the slot without
    // touching the surface. Releasing back at the SAME key leaves the
    // slot's key state machine unchanged; our own textures and the shared
    // section are process-owned and safe.
    if stop.load(Ordering::SeqCst) {
        let _ = unsafe { slot.mutex.ReleaseSync(key) };
        ring.policy.writer_abort(writer.index);
        if let Some(s) = &ring.section {
            s.reset_slot_free(writer.index);
        }
        return Ok(());
    }

    // Shared state is already WRITING via the take-CAS above.
    unsafe { context.CopyResource(&slot.texture, &frame_tex) };
    let release = unsafe { slot.mutex.ReleaseSync(luminal_driver_proto::KMTX_KEY_HOST) };

    match release {
        Ok(()) => {
            ring.ever_published[writer.index] = true;
            let seq = ring.policy.publish(writer.index);
            let present_qpc = if meta.PresentDisplayQPCTime != 0 {
                meta.PresentDisplayQPCTime
            } else {
                qpc_now()
            };
            if let Some(s) = &ring.section {
                s.slot_published(
                    writer.index,
                    seq,
                    present_qpc,
                    ring.policy.frames_published,
                    ring.policy.frames_dropped,
                );
            }
            Ok(())
        }
        Err(e) => {
            ring.policy.writer_abort(writer.index);
            if let Some(s) = &ring.section {
                s.reset_slot_free(writer.index);
            }
            Err(e)
        }
    }
}

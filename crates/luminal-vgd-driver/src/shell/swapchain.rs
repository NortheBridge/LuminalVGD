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
    /// Per-slot: has this slot ever been published this generation? A
    /// never-published slot's keyed mutex is still at its creation key
    /// (0, driver); after the first publish it lives at key 1 (host).
    ever_published: Vec<bool>,
    /// True once any assign has run — later assigns bump the generation
    /// (new device ⇒ new textures ⇒ new names).
    assigned_before: bool,
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

impl Worker {
    /// Bounded stop: the thread re-checks the flag at least every 100 ms.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
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
    if let Some(old) = rt.worker.take() {
        old.stop();
    }

    let ring = rt.ring.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let join = std::thread::spawn(move || {
        frame_loop(session_id, swapchain, frame_event, luid, ring, stop_thread)
    });
    rt.worker = Some(Worker { stop, join: Some(join) });
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

fn frame_loop(
    session_id: u64,
    swapchain: OsHandle,
    frame_event: OsHandle,
    luid: u64,
    ring: Arc<Mutex<FrameRing>>,
    stop: Arc<AtomicBool>,
) {
    tracelogging::write_event!(
        PROVIDER,
        "FrameWorkerSpawned",
        level(Informational),
        u64("session", &session_id)
    );
    let mut ring = ring.lock().unwrap();
    let ring = &mut *ring;

    // A reassignment means a new device: retire the old textures and bump
    // the generation so the host re-opens by name.
    if ring.assigned_before {
        ring.retire_textures();
    }
    ring.assigned_before = true;

    let mut d3d = match create_device_on_luid(luid) {
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
            return;
        }
    };

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
    if let Some(s) = &ring.section {
        s.set_state(ring_state::ACTIVE);
    }
    tracelogging::write_event!(
        PROVIDER,
        "FrameLoopStart",
        level(Informational),
        u64("session", &session_id),
        u32("generation", &ring.policy.generation)
    );

    let mut last_heartbeat = Instant::now();
    while !stop.load(Ordering::SeqCst) {
        if last_heartbeat.elapsed() >= HEARTBEAT_EVERY {
            if let Some(s) = &ring.section {
                s.heartbeat();
            }
            last_heartbeat = Instant::now();
        }

        let mut out: ffi::IDARG_OUT_RELEASEANDACQUIREBUFFER = unsafe { zeroed() };
        let status =
            unsafe { bindings::swapchain_release_and_acquire_buffer(swapchain.0.cast(), &mut out) };
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
            return;
        }

        // Frame in hand. pSurface stays valid until the next acquire.
        let meta = &out.MetaData;
        if meta.pSurface.is_null() {
            unsafe { bindings::swapchain_finished_processing_frame(swapchain.0.cast()) };
            continue;
        }
        let publish_result = publish_frame(session_id, ring, &d3d.0, &d3d.1, meta);
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
            return;
        }
    }
}

/// Copy the acquired frame into a ring slot and publish it. Any error is
/// returned after the ring bookkeeping is made consistent (abort/drop).
fn publish_frame(
    session_id: u64,
    ring: &mut FrameRing,
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    meta: &ffi::IDDCX_METADATA,
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
    // changed size. A size change is a full generation bump so the host
    // re-opens textures by name.
    if ring.textures.is_empty() || desc.Width != ring.tex_width || desc.Height != ring.tex_height {
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
        )?;
        ring.tex_width = desc.Width;
        ring.tex_height = desc.Height;
        tracelogging::write_event!(
            PROVIDER,
            "RingTexturesCreated",
            level(Informational),
            u64("session", &session_id),
            u32("generation", &ring.policy.generation),
            u32("width", &desc.Width),
            u32("height", &desc.Height)
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
            return Err(windows::core::Error::from_hresult(hr));
        }
    }

    if let Some(s) = &ring.section {
        s.slot_writing(writer.index);
    }
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

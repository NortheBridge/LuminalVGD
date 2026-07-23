// SPDX-License-Identifier: AGPL-3.0-only
//! Hardware cursor plane, driver side (DESIGN.md §3.2.3).
//!
//! IddCx delivers cursor shape/position through an event + query pull
//! model; this module republishes both into the per-monitor shared cursor
//! section (`CursorHeader` + 32bpp shape buffer, layout from
//! luminal-driver-proto) for the host to consume. One writer (the cursor
//! worker below), one reader (the host). §3.3 discipline: the worker's
//! only wait is a 100 ms-bounded event wait, so stop() is bounded; a
//! query failure drops that update and keeps the worker alive.
//!
//! Shape hand-off is a seqlock on `shape_generation`: odd = rewrite in
//! progress, even = stable. The host copies the buffer between two equal
//! even generation reads. Position updates touch only the position fields
//! (naturally-aligned u32 stores; a torn x/y pair across an update is one
//! frame of cursor lag, not corruption).

use core::ffi::c_void;
use core::mem::{size_of, zeroed};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS, PAGE_READWRITE,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use super::bindings::{self, ffi};
use super::ring::{qpc_now, with_ring_security};
use super::{OsHandle, PROVIDER};
use luminal_driver_proto::{
    cursor_kind, cursor_section_size, names, CursorHeader, CURSOR_HEADER_VERSION, CURSOR_MAGIC,
    CURSOR_MAX_DIM, CURSOR_SHAPE_OFFSET,
};

/// Writable mapping of the shared cursor section. Owned by the cursor
/// worker thread; the host maps the same bytes read-only.
struct CursorSection {
    mapping: HANDLE,
    view: *mut u8,
}

// SAFETY: single writer thread; all shared fields are written with
// volatile/atomic operations at their natural alignment.
unsafe impl Send for CursorSection {}

impl Drop for CursorSection {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(windows::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.view.cast::<c_void>(),
            });
            let _ = CloseHandle(self.mapping);
        }
    }
}

impl CursorSection {
    /// Create + map `Global\LuminalVGD-cur-<session>` and initialize the
    /// header (magic last — an incomplete section never looks valid).
    fn create(session_id: u64) -> windows::core::Result<Self> {
        let mut name = [0u16; 64];
        names::cursor_section_name(session_id, &mut name);
        let size = cursor_section_size();

        let mapping = with_ring_security(|sa| unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                Some(sa),
                PAGE_READWRITE,
                0,
                size as u32,
                PCWSTR(name.as_ptr()),
            )
        })?;
        let view = unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, size) };
        if view.Value.is_null() {
            let err = windows::core::Error::from_win32();
            unsafe {
                let _ = CloseHandle(mapping);
            }
            return Err(err);
        }
        let section = Self { mapping, view: view.Value.cast::<u8>() };
        unsafe {
            core::ptr::write_bytes(section.view, 0, size);
            let h = section.header_mut();
            core::ptr::write_volatile(&mut (*h).version, CURSOR_HEADER_VERSION);
            core::ptr::write_volatile(&mut (*h).magic, CURSOR_MAGIC);
        }
        Ok(section)
    }

    fn header_mut(&self) -> *mut CursorHeader {
        self.view.cast::<CursorHeader>()
    }

    fn generation_atomic(&self) -> &AtomicU32 {
        unsafe { AtomicU32::from_ptr(&mut (*self.header_mut()).shape_generation) }
    }

    /// Position/visibility update (header-only; see module docs on
    /// tearing).
    fn write_position(&self, x: i32, y: i32, visible: bool) {
        unsafe {
            let h = self.header_mut();
            core::ptr::write_volatile(&mut (*h).x, x);
            core::ptr::write_volatile(&mut (*h).y, y);
            core::ptr::write_volatile(&mut (*h).visible, visible as u32);
            core::ptr::write_volatile(&mut (*h).position_qpc, qpc_now());
        }
    }

    /// Seqlock shape rewrite: generation to odd, rewrite metadata + rows
    /// (compacted to a `width * 4` pitch), generation to next even.
    fn write_shape(
        &self,
        kind: u32,
        width: u32,
        height: u32,
        hotspot_x: u32,
        hotspot_y: u32,
        rows: &[u8],
        pitch: usize,
    ) {
        let generation = self.generation_atomic();
        let start = generation.load(Ordering::Relaxed);
        generation.store(start | 1, Ordering::Release);

        unsafe {
            let h = self.header_mut();
            core::ptr::write_volatile(&mut (*h).kind, kind);
            core::ptr::write_volatile(&mut (*h).width, width);
            core::ptr::write_volatile(&mut (*h).height, height);
            core::ptr::write_volatile(&mut (*h).hotspot_x, hotspot_x);
            core::ptr::write_volatile(&mut (*h).hotspot_y, hotspot_y);

            let row_bytes = (width as usize) * 4;
            let shape_base = self.view.add(CURSOR_SHAPE_OFFSET);
            for row in 0..height as usize {
                let src = &rows[row * pitch..row * pitch + row_bytes];
                core::ptr::copy_nonoverlapping(src.as_ptr(), shape_base.add(row * row_bytes), row_bytes);
            }
        }
        generation.store((start | 1).wrapping_add(1), Ordering::Release);
    }
}

pub(crate) struct CursorRt {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    /// Closed on drop — after departure, so the OS never signals a closed
    /// handle (drop the whole `MonitorRt` only once departure returned).
    event: OsHandle,
}

impl CursorRt {
    /// Bounded stop (the worker re-checks the flag at least every 100 ms).
    /// The event handle stays open until drop.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for CursorRt {
    fn drop(&mut self) {
        self.stop();
        unsafe {
            let _ = CloseHandle(HANDLE(self.event.0));
        }
    }
}

/// A cursor plane prepared but not yet claimed: the shared section exists
/// (created at plug so the host can map it early) while
/// `IddCxMonitorSetupHardwareCursor` still has to succeed.
pub(crate) struct CursorPending {
    section: CursorSection,
}

/// Setup phases, traced with every attempt so ETW shows exactly which
/// call site the OS accepts.
pub(crate) const PHASE_PLUG: u32 = 0;
pub(crate) const PHASE_ASSIGN: u32 = 1;

/// Create the shared cursor section at plug time. Failure is non-fatal
/// (traced): without a section there is no cursor plane and the OS keeps
/// composing the cursor into frames.
pub(crate) fn prepare(session_id: u64) -> Option<CursorPending> {
    match CursorSection::create(session_id) {
        Ok(section) => Some(CursorPending { section }),
        Err(e) => {
            let code = e.code().0;
            tracelogging::write_event!(
                PROVIDER,
                "CursorSectionCreateFailed",
                level(Error),
                u64("session", &session_id),
                i32("hresult", &code)
            );
            None
        }
    }
}

/// Try to claim the hardware cursor for `monitor` and spawn the worker.
/// Bring-up diagnosis mode: SetupHardwareCursor rejected the plug-time
/// call with STATUS_INVALID_PARAMETER (cursor caps are "for a given
/// path", and no path exists before the first mode commit), so attempts
/// run as a ladder — every (phase, variant, status) is traced, and the
/// assign phase also walks caps variants in case the rejection is about
/// the caps rather than the timing. `Err` hands the pending state back
/// for the next phase to retry.
pub(crate) fn try_setup(
    phase: u32,
    session_id: u64,
    monitor: OsHandle,
    pending: CursorPending,
) -> Result<CursorRt, CursorPending> {
    // Variant 0: alpha + XOR via OS emulation (out-of-band transport per
    // the header's own guidance). Variant 1: alpha + full XOR (the host
    // blend path handles masked shapes too). Variant 2: alpha only.
    const VARIANTS: [ffi::IDDCX_XOR_CURSOR_SUPPORT; 3] = [
        ffi::IDDCX_XOR_CURSOR_SUPPORT_IDDCX_XOR_CURSOR_SUPPORT_EMULATION,
        ffi::IDDCX_XOR_CURSOR_SUPPORT_IDDCX_XOR_CURSOR_SUPPORT_FULL,
        ffi::IDDCX_XOR_CURSOR_SUPPORT_IDDCX_XOR_CURSOR_SUPPORT_NONE,
    ];
    let variants: &[ffi::IDDCX_XOR_CURSOR_SUPPORT] = if phase == PHASE_PLUG {
        &VARIANTS[..1]
    } else {
        &VARIANTS[..]
    };

    let event = match unsafe { CreateEventW(None, false, false, None) } {
        Ok(h) => h,
        Err(e) => {
            let code = e.code().0;
            tracelogging::write_event!(
                PROVIDER,
                "CursorEventCreateFailed",
                level(Error),
                u64("session", &session_id),
                i32("hresult", &code)
            );
            return Err(pending);
        }
    };

    for (variant, &xor_support) in variants.iter().enumerate() {
        let mut caps: ffi::IDDCX_CURSOR_CAPS = unsafe { zeroed() };
        caps.Size = size_of::<ffi::IDDCX_CURSOR_CAPS>() as u32;
        caps.ColorXorCursorSupport = xor_support;
        caps.AlphaCursorSupport = 1;
        caps.MaxX = CURSOR_MAX_DIM;
        caps.MaxY = CURSOR_MAX_DIM;

        let mut in_args: ffi::IDARG_IN_SETUP_HWCURSOR = unsafe { zeroed() };
        in_args.CursorInfo = caps;
        in_args.hNewCursorDataAvailable = event.0.cast();

        let status =
            unsafe { bindings::monitor_setup_hardware_cursor(monitor.0.cast(), &in_args) };
        tracelogging::write_event!(
            PROVIDER,
            "CursorSetup",
            level(Informational),
            u64("session", &session_id),
            u32("phase", &phase),
            u32("variant", &(variant as u32)),
            i32("status", &status)
        );
        if status >= 0 {
            let stop = Arc::new(AtomicBool::new(false));
            let stop_thread = stop.clone();
            let event_os = OsHandle(event.0);
            let section = pending.section;
            let join = std::thread::spawn(move || {
                cursor_loop(session_id, monitor, event_os, section, stop_thread)
            });
            return Ok(CursorRt { stop, join: Some(join), event: OsHandle(event.0) });
        }
    }

    unsafe {
        let _ = CloseHandle(event);
    }
    Err(pending)
}

fn cursor_loop(
    session_id: u64,
    monitor: OsHandle,
    event: OsHandle,
    section: CursorSection,
    stop: Arc<AtomicBool>,
) {
    tracelogging::write_event!(
        PROVIDER,
        "CursorWorkerSpawned",
        level(Informational),
        u64("session", &session_id)
    );

    let mut shape_buf =
        vec![0u8; (CURSOR_MAX_DIM as usize) * (CURSOR_MAX_DIM as usize) * 4];
    let mut last_shape_id: u32 = 0;
    let mut last_query_failed = false;

    while !stop.load(Ordering::SeqCst) {
        let wait = unsafe { WaitForSingleObject(HANDLE(event.0), 100) };
        if wait != WAIT_OBJECT_0 {
            continue; // timeout — just the bounded stop check
        }

        let mut in_args: ffi::IDARG_IN_QUERY_HWCURSOR = unsafe { zeroed() };
        in_args.LastShapeId = last_shape_id;
        in_args.ShapeBufferSizeInBytes = shape_buf.len() as u32;
        in_args.pShapeBuffer = shape_buf.as_mut_ptr();
        let mut out: ffi::IDARG_OUT_QUERY_HWCURSOR = unsafe { zeroed() };
        out.CursorShapeInfo.Size = size_of::<ffi::IDDCX_CURSOR_SHAPE_INFO>() as u32;

        let status = unsafe {
            bindings::monitor_query_hardware_cursor(monitor.0.cast(), &in_args, &mut out)
        };
        if status < 0 {
            // Transient (e.g. mid-teardown): drop this update, stay alive.
            // Trace only edges so a wedged query can't flood ETW.
            if !last_query_failed {
                tracelogging::write_event!(
                    PROVIDER,
                    "CursorQueryFailed",
                    level(Warning),
                    u64("session", &session_id),
                    i32("status", &status)
                );
            }
            last_query_failed = true;
            continue;
        }
        last_query_failed = false;

        if out.IsCursorShapeUpdated != 0 {
            let info = &out.CursorShapeInfo;
            let width = info.Width.min(CURSOR_MAX_DIM);
            let height = info.Height.min(CURSOR_MAX_DIM);
            let kind = match info.CursorType {
                ffi::IDDCX_CURSOR_SHAPE_TYPE_IDDCX_CURSOR_SHAPE_TYPE_MASKED_COLOR => {
                    cursor_kind::MASKED
                }
                _ => cursor_kind::ALPHA,
            };
            let pitch = info.Pitch as usize;
            let needed = (height as usize).saturating_mul(pitch);
            if width > 0 && height > 0 && pitch >= (width as usize) * 4 && needed <= shape_buf.len()
            {
                section.write_shape(
                    kind,
                    width,
                    height,
                    info.XHot,
                    info.YHot,
                    &shape_buf[..needed],
                    pitch,
                );
                last_shape_id = info.ShapeId;
            }
        }

        section.write_position(out.X, out.Y, out.IsCursorVisible != 0);
    }
    tracelogging::write_event!(
        PROVIDER,
        "CursorWorkerExit",
        level(Informational),
        u64("session", &session_id)
    );
}

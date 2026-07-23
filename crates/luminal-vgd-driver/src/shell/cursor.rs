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
use std::time::Duration;

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
    /// Closed on drop unless the worker had to be detached (see stop()).
    event: OsHandle,
    detached: bool,
}

/// How long stop() waits for the worker before detaching it. Part of the
/// §3.3 teardown budget: a cursor query wedged inside the OS must never
/// extend monitor departure (and with it the whole control plane)
/// unboundedly.
const STOP_DEADLINE: Duration = Duration::from_millis(500);

impl CursorRt {
    /// Deadline-bounded stop. The worker re-checks the flag at least
    /// every 100 ms; if it fails to exit within [`STOP_DEADLINE`] (an OS
    /// call wedged), the thread is detached and the event handle is
    /// deliberately leaked so the stuck call never touches a closed
    /// handle. Teardown proceeds regardless.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let Some(join) = self.join.take() else { return };
        let deadline = std::time::Instant::now() + STOP_DEADLINE;
        while !join.is_finished() {
            if std::time::Instant::now() >= deadline {
                tracelogging::write_event!(
                    PROVIDER,
                    "CursorStopTimeout",
                    level(Warning)
                );
                self.detached = true;
                drop(join); // detach; the worker exits on its own if it ever unblocks
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = join.join();
    }
}

impl Drop for CursorRt {
    fn drop(&mut self) {
        self.stop();
        if !self.detached {
            unsafe {
                let _ = CloseHandle(HANDLE(self.event.0));
            }
        }
        // Detached: event handle leaks intentionally (worker may still be
        // blocked on it inside the OS).
    }
}

/// Claim the cursor plane for `monitor`: create the shared section and
/// spawn the worker. ALL cursor IddCx calls (SetupHardwareCursor and the
/// queries) happen on the worker thread — never on the plug/ioctl path
/// and never inside an IddCx callback. IddCx callbacks are win32k
/// callouts; calling back into IddCx from one can deadlock against the
/// locks win32k holds while calling us, and the win32k callout watchdog
/// then fires LiveKernelEvent 0x1b8 storms until the machine wedges
/// (observed live, 2026-07-23). The worker retries SetupHardwareCursor
/// on its own clock until a path is committed and the OS accepts.
pub(crate) fn spawn(session_id: u64, monitor: OsHandle) -> Option<CursorRt> {
    let section = match CursorSection::create(session_id) {
        Ok(s) => s,
        Err(e) => {
            let code = e.code().0;
            tracelogging::write_event!(
                PROVIDER,
                "CursorSectionCreateFailed",
                level(Error),
                u64("session", &session_id),
                i32("hresult", &code)
            );
            return None;
        }
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
            return None;
        }
    };

    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let event_os = OsHandle(event.0);
    let join = std::thread::spawn(move || {
        cursor_loop(session_id, monitor, event_os, section, stop_thread)
    });
    Some(CursorRt { stop, join: Some(join), event: OsHandle(event.0), detached: false })
}

/// One SetupHardwareCursor attempt ladder (EMULATION → FULL → NONE XOR
/// caps). Returns the accepted variant, or the last status. Runs on the
/// worker thread only.
fn setup_attempt(
    session_id: u64,
    monitor: OsHandle,
    event: OsHandle,
    round: u32,
    last_status: &mut i32,
) -> bool {
    const VARIANTS: [ffi::IDDCX_XOR_CURSOR_SUPPORT; 3] = [
        ffi::IDDCX_XOR_CURSOR_SUPPORT_IDDCX_XOR_CURSOR_SUPPORT_EMULATION,
        ffi::IDDCX_XOR_CURSOR_SUPPORT_IDDCX_XOR_CURSOR_SUPPORT_FULL,
        ffi::IDDCX_XOR_CURSOR_SUPPORT_IDDCX_XOR_CURSOR_SUPPORT_NONE,
    ];
    for (variant, &xor_support) in VARIANTS.iter().enumerate() {
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
        // Trace edges (status changes) and successes — the retry loop
        // runs at 1 Hz until a path commits, which would flood ETW.
        if status >= 0 || status != *last_status {
            tracelogging::write_event!(
                PROVIDER,
                "CursorSetup",
                level(Informational),
                u64("session", &session_id),
                u32("round", &round),
                u32("variant", &(variant as u32)),
                i32("status", &status)
            );
        }
        *last_status = status;
        if status >= 0 {
            return true;
        }
    }
    false
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

    // Phase 1: claim the plane. SetupHardwareCursor is rejected with
    // INVALID_PARAMETER until a path is committed on the monitor (mode
    // commit happens seconds after plug), so retry at 1 Hz on this
    // thread's own clock — never from plug or an IddCx callback.
    let mut setup_round: u32 = 0;
    let mut last_setup_status: i32 = 0;
    loop {
        if stop.load(Ordering::SeqCst) {
            tracelogging::write_event!(
                PROVIDER,
                "CursorWorkerExit",
                level(Informational),
                u64("session", &session_id)
            );
            return;
        }
        if setup_attempt(session_id, monitor, event, setup_round, &mut last_setup_status) {
            break;
        }
        setup_round += 1;
        std::thread::sleep(Duration::from_millis(if setup_round < 3 { 250 } else { 1000 }));
    }

    let mut shape_buf =
        vec![0u8; (CURSOR_MAX_DIM as usize) * (CURSOR_MAX_DIM as usize) * 4];
    let mut last_shape_id: u32 = 0;
    let mut last_query_failed = false;
    // Which QueryHardwareCursor variant this OS accepts: 0 = not yet
    // discovered; then 3/2/1. The FP16 (HDR) adapter contract rejects v1
    // with STATUS_NOT_SUPPORTED (ETW-confirmed on build 4), so discovery
    // walks newest → oldest and latches the first success.
    let mut query_version: u32 = 0;
    // v2/v3 report X/Y only while PositionValid; carry the last good pair.
    let mut last_x: i32 = 0;
    let mut last_y: i32 = 0;

    /// One query's OS reply, unified across the three variants.
    struct Update {
        visible: bool,
        x: i32,
        y: i32,
        position_valid: bool,
        shape_updated: bool,
        info: ffi::IDDCX_CURSOR_SHAPE_INFO,
    }

    while !stop.load(Ordering::SeqCst) {
        let wait = unsafe { WaitForSingleObject(HANDLE(event.0), 100) };
        if wait != WAIT_OBJECT_0 {
            continue; // timeout — just the bounded stop check
        }
        // Re-check after the wait: stop() gives teardown a 500 ms deadline
        // before departure, so refusing to issue a query once stopping
        // guarantees no cursor query ever targets a departed monitor.
        if stop.load(Ordering::SeqCst) {
            break;
        }

        let mut in_args: ffi::IDARG_IN_QUERY_HWCURSOR = unsafe { zeroed() };
        in_args.LastShapeId = last_shape_id;
        in_args.ShapeBufferSizeInBytes = shape_buf.len() as u32;
        in_args.pShapeBuffer = shape_buf.as_mut_ptr();

        let query = |version: u32| -> (i32, Option<Update>) {
            match version {
                3 => {
                    let mut out: ffi::IDARG_OUT_QUERY_HWCURSOR3 = unsafe { zeroed() };
                    out.CursorShapeInfo.Size = size_of::<ffi::IDDCX_CURSOR_SHAPE_INFO>() as u32;
                    let status = unsafe {
                        bindings::monitor_query_hardware_cursor3(monitor.0.cast(), &in_args, &mut out)
                    };
                    let update = (status >= 0).then(|| Update {
                        visible: out.IsCursorVisible != 0,
                        x: out.X,
                        y: out.Y,
                        position_valid: out.PositionValid != 0,
                        shape_updated: out.IsCursorShapeUpdated != 0,
                        info: out.CursorShapeInfo,
                    });
                    (status, update)
                }
                2 => {
                    let mut out: ffi::IDARG_OUT_QUERY_HWCURSOR2 = unsafe { zeroed() };
                    out.CursorShapeInfo.Size = size_of::<ffi::IDDCX_CURSOR_SHAPE_INFO>() as u32;
                    let status = unsafe {
                        bindings::monitor_query_hardware_cursor2(monitor.0.cast(), &in_args, &mut out)
                    };
                    let update = (status >= 0).then(|| Update {
                        visible: out.IsCursorVisible != 0,
                        x: out.X,
                        y: out.Y,
                        position_valid: out.PositionValid != 0,
                        shape_updated: out.IsCursorShapeUpdated != 0,
                        info: out.CursorShapeInfo,
                    });
                    (status, update)
                }
                _ => {
                    let mut out: ffi::IDARG_OUT_QUERY_HWCURSOR = unsafe { zeroed() };
                    out.CursorShapeInfo.Size = size_of::<ffi::IDDCX_CURSOR_SHAPE_INFO>() as u32;
                    let status = unsafe {
                        bindings::monitor_query_hardware_cursor(monitor.0.cast(), &in_args, &mut out)
                    };
                    let update = (status >= 0).then(|| Update {
                        visible: out.IsCursorVisible != 0,
                        x: out.X,
                        y: out.Y,
                        position_valid: true, // v1 X/Y are always populated
                        shape_updated: out.IsCursorShapeUpdated != 0,
                        info: out.CursorShapeInfo,
                    });
                    (status, update)
                }
            }
        };

        let (status, update) = if query_version != 0 {
            query(query_version)
        } else {
            // Discovery: newest → oldest, latch the first accepted variant.
            let mut result = (-1, None);
            for version in [3u32, 2, 1] {
                result = query(version);
                if result.0 >= 0 {
                    query_version = version;
                    tracelogging::write_event!(
                        PROVIDER,
                        "CursorQueryMode",
                        level(Informational),
                        u64("session", &session_id),
                        u32("version", &version)
                    );
                    break;
                }
            }
            result
        };

        let Some(update) = update else {
            // Transient (e.g. mid-teardown): drop this update, stay alive.
            // Trace only edges so a wedged query can't flood ETW.
            if !last_query_failed {
                tracelogging::write_event!(
                    PROVIDER,
                    "CursorQueryFailed",
                    level(Warning),
                    u64("session", &session_id),
                    u32("version", &query_version),
                    i32("status", &status)
                );
            }
            last_query_failed = true;
            continue;
        };
        last_query_failed = false;

        if update.shape_updated {
            let info = &update.info;
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

        if update.position_valid {
            last_x = update.x;
            last_y = update.y;
        }
        section.write_position(last_x, last_y, update.visible);
    }
    tracelogging::write_event!(
        PROVIDER,
        "CursorWorkerExit",
        level(Informational),
        u64("session", &session_id)
    );
}

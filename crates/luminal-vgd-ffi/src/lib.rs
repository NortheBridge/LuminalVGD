// SPDX-License-Identifier: AGPL-3.0-only
//! C ABI for LuminalShine (C++): control-device operations and ring
//! consumption, wrapping `luminal-vgd-host`. This is a *conversion*
//! boundary — the host↔driver wire ABI still lives only in
//! `luminal-driver-proto`; the `Vgd*` structs here exist so cbindgen can
//! emit a self-contained C header.
//!
//! Conventions:
//! - Handles are opaque pointers (`VgdDeviceHandle`, `VgdRingHandle`);
//!   every `*_open` has exactly one `*_close`.
//! - Functions return `0` on success, a negative `err::*` proto code on
//!   driver-refused, or [`VGD_ERR_IO`] on OS-level failure.
//! - All entry points are panic-proof (`catch_unwind`): a bug in this
//!   layer degrades to an error code, never unwinds into C++.
//!
//! Texture access stays on the C++ side: claim a frame, compose the
//! texture name with [`vgd_slot_texture_name`], `OpenSharedResourceByName`
//! on the encoder's D3D11 device, keyed-mutex acquire key 1 (bounded!),
//! use, release to key 1, then [`vgd_ring_release`].

#![cfg(windows)]
#![allow(clippy::missing_safety_doc)]

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::null_mut;

use luminal_driver_proto::{names, CreateMonitorRequest, ModeSpec, MAX_MODES_PER_MONITOR};
use luminal_vgd_host::device::{CursorView, RingView, VgdDevice};

/// OS-level failure (I/O error, device unreachable) as opposed to a
/// negative proto `err::*` code from the driver.
pub const VGD_ERR_IO: i32 = -1000;

/// Capability bits of `VgdCaps.caps` the backend gates on (literal
/// mirrors of proto `caps::*`; the asserts below keep them honest —
/// cbindgen cannot evaluate cross-crate constants).
pub const VGD_CAP_HDR10: u32 = 1;
pub const VGD_CAP_SDR10_BIT: u32 = 4;
pub const VGD_CAP_HW_CURSOR: u32 = 32;
/// The driver implements [`vgd_update_modes`] (proto 0.5, driver build
/// 17+). Absent ⇒ a monitor's mode list is fixed at create time and the
/// caller must advertise everything it may need up front.
pub const VGD_CAP_DYNAMIC_MODES: u32 = 512;
pub const VGD_CAP_D3D12_FENCE_TRANSPORT: u32 = 1024;
pub const VGD_CAP_FENCE_TRANSPORT_REQUIRED: u32 = 2048;
pub const VGD_CREATE_D3D12_FENCE_TRANSPORT: u32 = 4;
pub const VGD_CREATE_FENCE_TRANSPORT_REQUIRED: u32 = 8;
const _: () = assert!(VGD_CAP_HDR10 == luminal_driver_proto::caps::HDR10);
const _: () = assert!(VGD_CAP_SDR10_BIT == luminal_driver_proto::caps::SDR10_BIT);
const _: () = assert!(VGD_CAP_HW_CURSOR == luminal_driver_proto::caps::HW_CURSOR);
const _: () = assert!(VGD_CAP_DYNAMIC_MODES == luminal_driver_proto::caps::DYNAMIC_MODES);
const _: () = assert!(
    VGD_CAP_D3D12_FENCE_TRANSPORT == luminal_driver_proto::caps::D3D12_FENCE_TRANSPORT
);
const _: () = assert!(
    VGD_CREATE_D3D12_FENCE_TRANSPORT == luminal_driver_proto::create_flags::D3D12_FENCE_TRANSPORT
);
const _: () = assert!(
    VGD_CAP_FENCE_TRANSPORT_REQUIRED == luminal_driver_proto::caps::FENCE_TRANSPORT_REQUIRED
);
const _: () = assert!(
    VGD_CREATE_FENCE_TRANSPORT_REQUIRED == luminal_driver_proto::create_flags::FENCE_TRANSPORT_REQUIRED
);

/// `VgdCursorShape.kind` values (mirror proto `cursor_kind::*`).
pub const VGD_CURSOR_KIND_ALPHA: u32 = 1;
pub const VGD_CURSOR_KIND_MASKED: u32 = 3;
const _: () = assert!(VGD_CURSOR_KIND_ALPHA == luminal_driver_proto::cursor_kind::ALPHA);
const _: () = assert!(VGD_CURSOR_KIND_MASKED == luminal_driver_proto::cursor_kind::MASKED);

/// Worst-case shape buffer size for `vgd_cursor_shape` (256² 32bpp).
pub const VGD_CURSOR_SHAPE_BUFFER_SIZE: u32 = 256 * 256 * 4;
const _: () = assert!(
    VGD_CURSOR_SHAPE_BUFFER_SIZE as usize
        == luminal_driver_proto::cursor_section_size() - luminal_driver_proto::CURSOR_SHAPE_OFFSET
);

pub struct VgdDeviceHandle(VgdDevice);
pub struct VgdRingHandle(RingView);
pub struct VgdCursorHandle(CursorView);

/// Handshake results the backend needs for capability gating.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VgdCaps {
    pub proto_major: u16,
    pub proto_minor: u16,
    pub driver_build: u32,
    pub caps: u32,
    pub max_monitors: u32,
    pub watchdog_secs: u32,
}

/// One display mode; `modes[0]` is preferred. Mirrors proto `ModeSpec`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VgdModeSpec {
    pub width: u32,
    pub height: u32,
    pub refresh_millihz: u32,
}

/// Monitor creation parameters (see proto `CreateMonitorRequest` for full
/// field semantics; zero means "driver default" where the proto says so).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VgdCreateRequest {
    pub session_id: u64,
    pub display_id: u64,
    pub adapter_luid: u64,
    pub lease_timeout_ms: u32,
    pub bit_depth: u32,
    pub hdr: u32,
    pub flags: u32,
    pub mode_count: u32,
    pub modes: [VgdModeSpec; 4],
    pub physical_width_mm: u32,
    pub physical_height_mm: u32,
    /// NUL-padded UTF-16LE.
    pub friendly_name: [u16; 32],
    /// Desired HDR peak luminance in nits for the monitor EDID's
    /// CTA-861.3 block. 0 = driver default (≈993 nits). Ignored for SDR
    /// monitors and by pre-0.4 drivers (proto 0.4 additive field).
    pub max_nits: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VgdCreateReply {
    pub session_id: u64,
    pub display_id: u64,
    /// `0` or a negative proto `err::*` code.
    pub result: i32,
    pub ring_slots: u32,
    pub connector_index: u32,
}

/// Snapshot of the ring header for health/fallback decisions.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VgdRingStatus {
    pub generation: u32,
    /// proto `ring_state::*` (1 ACTIVE, 2 REBUILDING, 3 DEAD, 0 uninit).
    pub state: u32,
    /// Transport flags actually selected for this generation.
    pub transport_flags: u32,
    pub reserved: u32,
    pub latest_sequence: u64,
    pub frames_published: u64,
    pub frames_dropped: u64,
    pub heartbeat_qpc: u64,
    pub qpc_frequency: u64,
}

/// A claimed (checked-out) frame. The driver will not overwrite this slot
/// until `vgd_ring_release`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VgdFrame {
    pub index: u32,
    /// Bake into the texture name; re-claim if the header generation has
    /// moved on by the time the texture is opened.
    pub generation: u32,
    pub sequence: u64,
    pub present_qpc: u64,
    pub ready_fence_value: u64,
}

fn guarded<T>(default: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(default)
}

std::thread_local! {
    /// OS error of the most recent failing FFI call on this thread (0 =
    /// none). The 2026-07 streaming outage was undiagnosable from host
    /// logs because every failure collapsed to NULL/`VGD_ERR_IO`; this
    /// carries the underlying Win32 error across the boundary.
    static LAST_OS_ERROR: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn record_os_error(e: &std::io::Error) {
    LAST_OS_ERROR.with(|c| c.set(e.raw_os_error().unwrap_or(0) as u32));
}

/// Win32 error code of the most recent failing FFI call on the calling
/// thread (0 = none recorded). Read it immediately after a NULL /
/// [`VGD_ERR_IO`] return; success does not clear it.
#[no_mangle]
pub extern "C" fn vgd_last_error() -> u32 {
    LAST_OS_ERROR.with(|c| c.get())
}

/// Open the LuminalVGD control device. NULL when the driver is absent —
/// the caller falls back to another backend.
#[no_mangle]
pub extern "C" fn vgd_device_open() -> *mut VgdDeviceHandle {
    guarded(null_mut(), || match VgdDevice::open_first() {
        Ok(dev) => Box::into_raw(Box::new(VgdDeviceHandle(dev))),
        Err(e) => {
            record_os_error(&e);
            null_mut()
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn vgd_device_close(dev: *mut VgdDeviceHandle) {
    if !dev.is_null() {
        drop(Box::from_raw(dev));
    }
}

#[no_mangle]
pub unsafe extern "C" fn vgd_handshake(dev: *mut VgdDeviceHandle, out: *mut VgdCaps) -> i32 {
    if dev.is_null() || out.is_null() {
        return VGD_ERR_IO;
    }
    guarded(VGD_ERR_IO, || match (*dev).0.handshake() {
        Ok(h) => {
            *out = VgdCaps {
                proto_major: h.driver_proto_major,
                proto_minor: h.driver_proto_minor,
                driver_build: h.driver_build,
                caps: h.caps,
                max_monitors: h.max_monitors,
                watchdog_secs: h.watchdog_secs,
            };
            0
        }
        Err(e) => {
            record_os_error(&e);
            VGD_ERR_IO
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn vgd_create_monitor(
    dev: *mut VgdDeviceHandle,
    req: *const VgdCreateRequest,
    out: *mut VgdCreateReply,
) -> i32 {
    if dev.is_null() || req.is_null() || out.is_null() {
        return VGD_ERR_IO;
    }
    guarded(VGD_ERR_IO, || {
        let r = &*req;
        let mut modes = [ModeSpec::default(); MAX_MODES_PER_MONITOR as usize];
        for (dst, src) in modes.iter_mut().zip(r.modes.iter()) {
            *dst = ModeSpec {
                width: src.width,
                height: src.height,
                refresh_millihz: src.refresh_millihz,
            };
        }
        let proto_req = CreateMonitorRequest {
            session_id: r.session_id,
            display_id: r.display_id,
            adapter_luid: r.adapter_luid,
            lease_timeout_ms: r.lease_timeout_ms,
            bit_depth: r.bit_depth,
            hdr: r.hdr,
            edid_serial: 0,
            flags: r.flags,
            max_nits: r.max_nits,
            reserved0: 0,
            mode_count: r.mode_count,
            modes,
            physical_width_mm: r.physical_width_mm,
            physical_height_mm: r.physical_height_mm,
            friendly_name: r.friendly_name,
        };
        match (*dev).0.create_monitor(&proto_req) {
            Ok(reply) => {
                *out = VgdCreateReply {
                    session_id: reply.session_id,
                    display_id: reply.display_id,
                    result: reply.result,
                    ring_slots: reply.ring_slots,
                    connector_index: reply.connector_index,
                };
                0
            }
            Err(e) => {
                record_os_error(&e);
                VGD_ERR_IO
            }
        }
    })
}

/// Parameters for [`vgd_update_modes`] (mirrors proto
/// `UpdateModesRequest`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VgdUpdateModesRequest {
    pub session_id: u64,
    /// Reserved; pass 0.
    pub flags: u32,
    /// Valid entries in `modes` (1..=4). Zero is refused — the offered
    /// list can be replaced, never emptied.
    pub mode_count: u32,
    /// The complete set of modes the monitor should offer from now on.
    /// Every entry must be one the monitor was CREATED with.
    pub modes: [VgdModeSpec; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VgdUpdateModesReply {
    pub session_id: u64,
    /// `0` (accepted) or a negative proto `err::*` code.
    pub result: i32,
    /// Modes the monitor offers once this request is applied.
    pub mode_count: u32,
    /// Requested modes the driver published. `accepted < requested` means
    /// the rest are modes this monitor was not created with and can never
    /// offer: partial application, NOT an error, and nothing that existed
    /// was discarded.
    pub accepted: u32,
    /// Echo of the request's `mode_count`.
    pub requested: u32,
    /// `VGD_UPDATE_PENDING` / `VGD_UPDATE_PARTIAL`.
    pub flags: u32,
    /// Requested modes with no counterpart in the monitor's create-time
    /// list. `accepted + rejected == requested` always.
    pub rejected: u32,
    /// Index into `modes` of the first rejected entry, or
    /// [`VGD_NO_REJECTED_INDEX`] — so a caller can name the mode it
    /// cannot have instead of only counting it.
    pub first_rejected: u32,
    /// With [`VGD_UPDATE_BLOCKED`] set: index into the mode list this
    /// monitor was CREATED with, naming the mode the OS is currently
    /// running and that this request would have taken away. Otherwise
    /// [`VGD_NO_MODE_INDEX`].
    ///
    /// This is the one field that turns "no" into something actionable:
    /// ask again with a subset that KEEPS this mode, or move the display
    /// onto the mode you want first and gate afterwards.
    pub blocking_mode: u32,
}

/// [`VgdUpdateModesReply::first_rejected`] when nothing was rejected.
/// Distinct from index 0, which is a real rejection of the first mode.
pub const VGD_NO_REJECTED_INDEX: u32 = u32::MAX;
const _: () = assert!(VGD_NO_REJECTED_INDEX == luminal_driver_proto::NO_REJECTED_INDEX);

/// [`VgdUpdateModesReply::flags`]: the list is not in force yet — the
/// driver queued the OS-side push, which cannot have run before this call
/// returned. A push that fails or is deferred leaves the PREVIOUS list in
/// force and the request fully retryable (resending it really does push
/// again), and surfaces as the monitor's sticky `last_error` (`-13`) in
/// the status reply.
pub const VGD_UPDATE_PENDING: u32 = 1;
/// [`VgdUpdateModesReply::flags`]: fewer modes accepted than requested,
/// because some requested modes are not in the monitor's create-time list
/// and can never be offered. Success with detail — everything that exists
/// was published. See [`VgdUpdateModesReply::rejected`].
pub const VGD_UPDATE_PARTIAL: u32 = 2;
/// [`VgdUpdateModesReply::flags`]: **the request was refused permanently —
/// do not retry it.** The list would have taken away the mode the OS is
/// currently running on this display, which would force a mode change on a
/// live monitor mid-stream, so the driver refused the push and left the
/// previously offered list in force (`result` = `-14`).
///
/// Retrying is the one response that cannot work: the answer depends on
/// what the display is running, not on the attempt. Read
/// [`VgdUpdateModesReply::blocking_mode`] and either include that mode in
/// the next request or change the display mode first.
pub const VGD_UPDATE_BLOCKED: u32 = 4;
const _: () = assert!(VGD_UPDATE_PENDING == luminal_driver_proto::update_status::PENDING);
const _: () = assert!(VGD_UPDATE_PARTIAL == luminal_driver_proto::update_status::PARTIAL);
const _: () = assert!(VGD_UPDATE_BLOCKED == luminal_driver_proto::update_status::BLOCKED);

/// [`VgdUpdateModesReply::blocking_mode`] when nothing is blocking.
/// Distinct from index 0, which names the monitor's first create-time mode.
pub const VGD_NO_MODE_INDEX: u32 = u32::MAX;
const _: () = assert!(VGD_NO_MODE_INDEX == luminal_driver_proto::NO_MODE_INDEX);

/// Change WHICH of a LIVE monitor's create-time modes it currently
/// offers — no destroy/create cycle, so no `DBT_DEVNODES_CHANGED`
/// broadcast and no monitor churn (proto 0.5, driver build 17+).
///
/// The motivating case: a client streams a display whose create call
/// listed both its base refresh rate and the frame-generation-doubled
/// one, and only then launches a framegen title. Calling this with
/// `{doubled}` makes the doubled rate the one on offer, and calling it
/// again with `{base, doubled}` (or `{base}`) puts things back.
///
/// # Contract the caller must honor
///
/// - **Gate on `VgdCaps.caps & VGD_CAP_DYNAMIC_MODES`.** Against an older
///   driver the opcode is unknown and this returns [`VGD_ERR_IO`]; that
///   is safe (never a false success) but it is a wasted round trip, and
///   the log line the caller writes should name the driver's version.
/// - **You can only choose among the modes you CREATED the monitor
///   with.** A monitor's description is frozen at creation — no IddCx DDI
///   replaces it on a live monitor, and Windows only offers modes that
///   are in BOTH the description and the pushed list — so a mode that was
///   not in `vgd_create_monitor`'s list is rejected here
///   (`VGD_UPDATE_PARTIAL`, or `result` = `-3` if nothing requested
///   exists). Create with every mode you might later want.
/// - **The list is replaced, never emptied.** `mode_count == 0` is
///   refused; so is a request in which every mode is unknown to the
///   monitor. In both cases the previously offered list stays in force.
/// - **Degrade, never refuse.** Treat both `VGD_ERR_IO` and any negative
///   `result` as "the offered modes are unchanged, carry on with the
///   session". A failed update is never a reason to tear a stream down.
/// - **Do not retry a [`VGD_UPDATE_BLOCKED`] refusal** (`result` = `-14`).
///   Every other failure here leaves the previous list in force and really
///   does push again on the next attempt, so retrying is reasonable. This
///   one is a statement about the mode the display is RUNNING: the driver
///   refused to take that mode away mid-stream, and will refuse again until
///   the display is on something else. `blocking_mode` indexes your own
///   `vgd_create_monitor` list and names it — either include that mode in
///   the next request, or change the display mode yourself first (the
///   modeset is then yours, at a moment you chose) and gate afterwards.
/// - **`result == 0` means ACCEPTED, not applied.** The driver completes
///   the request before it calls the OS. The applied/failed outcome shows
///   up in ETW and as the monitor's sticky `last_error` in the status
///   reply. `result == 0` with neither flag set is the only shape that
///   means "every mode you asked for is offered right now"; with
///   `VGD_UPDATE_PENDING` set, an update is still outstanding, and if it
///   does not land the previous list stays in force and this exact
///   request can simply be sent again.
/// - **Check `accepted` against `requested`**, and read `rejected` /
///   `first_rejected` when they differ: that names a mode this monitor
///   will never offer, which is a create-time problem, not a retryable
///   one.
#[no_mangle]
pub unsafe extern "C" fn vgd_update_modes(
    dev: *mut VgdDeviceHandle,
    req: *const VgdUpdateModesRequest,
    out: *mut VgdUpdateModesReply,
) -> i32 {
    if dev.is_null() || req.is_null() || out.is_null() {
        return VGD_ERR_IO;
    }
    guarded(VGD_ERR_IO, || {
        let r = &*req;
        let mut modes = [ModeSpec::default(); MAX_MODES_PER_MONITOR as usize];
        for (dst, src) in modes.iter_mut().zip(r.modes.iter()) {
            *dst = ModeSpec {
                width: src.width,
                height: src.height,
                refresh_millihz: src.refresh_millihz,
            };
        }
        let proto_req = luminal_driver_proto::UpdateModesRequest {
            session_id: r.session_id,
            flags: r.flags,
            mode_count: r.mode_count,
            modes,
            reserved: [0; 4],
        };
        match (*dev).0.update_modes(&proto_req) {
            Ok(reply) => {
                *out = VgdUpdateModesReply {
                    session_id: reply.session_id,
                    result: reply.result,
                    mode_count: reply.mode_count,
                    accepted: reply.accepted(),
                    requested: reply.requested(),
                    flags: reply.flags(),
                    rejected: reply.rejected(),
                    first_rejected: reply.first_rejected(),
                    blocking_mode: reply.blocking_mode_idx(),
                };
                0
            }
            Err(e) => {
                record_os_error(&e);
                VGD_ERR_IO
            }
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn vgd_destroy_monitor(dev: *mut VgdDeviceHandle, session_id: u64) -> i32 {
    if dev.is_null() {
        return VGD_ERR_IO;
    }
    guarded(VGD_ERR_IO, || {
        (*dev).0.destroy_monitor(session_id).unwrap_or(VGD_ERR_IO)
    })
}

/// Feed the per-lease watchdog; call at least once per `watchdog_secs`.
#[no_mangle]
pub unsafe extern "C" fn vgd_ping(dev: *mut VgdDeviceHandle, session_id: u64) -> i32 {
    if dev.is_null() {
        return VGD_ERR_IO;
    }
    guarded(VGD_ERR_IO, || (*dev).0.ping(session_id).unwrap_or(VGD_ERR_IO))
}

/// Map the frame ring for a created monitor. NULL until the driver has
/// created the section (retry briefly after create).
#[no_mangle]
pub extern "C" fn vgd_ring_open(session_id: u64, ring_slots: u32) -> *mut VgdRingHandle {
    guarded(null_mut(), || match RingView::open(session_id, ring_slots) {
        Ok(view) => Box::into_raw(Box::new(VgdRingHandle(view))),
        Err(e) => {
            record_os_error(&e);
            null_mut()
        }
    })
}

#[no_mangle]
pub unsafe extern "C" fn vgd_ring_close(ring: *mut VgdRingHandle) {
    if !ring.is_null() {
        drop(Box::from_raw(ring));
    }
}

#[no_mangle]
pub unsafe extern "C" fn vgd_ring_status(ring: *mut VgdRingHandle, out: *mut VgdRingStatus) -> i32 {
    if ring.is_null() || out.is_null() {
        return VGD_ERR_IO;
    }
    guarded(VGD_ERR_IO, || {
        let h = (*ring).0.header();
        *out = VgdRingStatus {
            generation: h.ring_generation,
            state: h.state,
            transport_flags: h.transport_flags(),
            reserved: 0,
            latest_sequence: h.latest_sequence,
            frames_published: h.frames_published,
            frames_dropped: h.frames_dropped,
            heartbeat_qpc: h.driver_heartbeat_qpc,
            qpc_frequency: h.qpc_frequency,
        };
        0
    })
}

/// Claim the freshest published frame. Returns `true` and fills `out`
/// when a frame was checked out; `false` when nothing is published.
#[no_mangle]
pub unsafe extern "C" fn vgd_ring_claim(ring: *mut VgdRingHandle, out: *mut VgdFrame) -> bool {
    if ring.is_null() || out.is_null() {
        return false;
    }
    guarded(false, || match (*ring).0.claim_latest() {
        Some(frame) => {
            *out = VgdFrame {
                index: frame.index,
                generation: frame.generation,
                sequence: frame.sequence,
                present_qpc: frame.present_qpc,
                ready_fence_value: frame.ready_fence_value,
            };
            true
        }
        None => false,
    })
}

/// Release a claimed frame back to the driver. Exactly once per claim.
#[no_mangle]
pub unsafe extern "C" fn vgd_ring_release(ring: *mut VgdRingHandle, index: u32) {
    if !ring.is_null() {
        guarded((), || (*ring).0.release(index));
    }
}

/// Cursor position/visibility snapshot (`vgd_cursor_state`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VgdCursorState {
    /// Desktop coordinates of the shape's top-left pixel (can be
    /// negative when the hotspot hangs off the display edge).
    pub x: i32,
    pub y: i32,
    /// 0 hidden, 1 visible.
    pub visible: u32,
    /// Even counter bumped after each complete shape rewrite (0 = no
    /// shape yet). Re-fetch the shape when it changes.
    pub shape_generation: u32,
    pub position_qpc: u64,
}

/// Cursor shape metadata; pixels land in the caller's buffer at a
/// `width * 4` pitch (32bpp, `VGD_CURSOR_KIND_*`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VgdCursorShape {
    pub kind: u32,
    pub width: u32,
    pub height: u32,
    pub hotspot_x: u32,
    pub hotspot_y: u32,
    /// The generation this copy is valid for.
    pub generation: u32,
}

/// Map the shared cursor section for a created monitor (requires
/// `VGD_CAP_HW_CURSOR`). NULL when the driver has no cursor plane for
/// this monitor — the cursor is then composed into frames as before.
#[no_mangle]
pub extern "C" fn vgd_cursor_open(session_id: u64) -> *mut VgdCursorHandle {
    guarded(null_mut(), || match CursorView::open(session_id) {
        Ok(view) => Box::into_raw(Box::new(VgdCursorHandle(view))),
        Err(_) => null_mut(),
    })
}

#[no_mangle]
pub unsafe extern "C" fn vgd_cursor_close(cursor: *mut VgdCursorHandle) {
    if !cursor.is_null() {
        drop(Box::from_raw(cursor));
    }
}

/// Position/visibility snapshot (cheap; poll every frame).
#[no_mangle]
pub unsafe extern "C" fn vgd_cursor_state(
    cursor: *mut VgdCursorHandle,
    out: *mut VgdCursorState,
) -> i32 {
    if cursor.is_null() || out.is_null() {
        return VGD_ERR_IO;
    }
    guarded(VGD_ERR_IO, || {
        let s = (*cursor).0.state();
        *out = VgdCursorState {
            x: s.x,
            y: s.y,
            visible: s.visible as u32,
            shape_generation: s.shape_generation,
            position_qpc: s.position_qpc,
        };
        0
    })
}

/// Copy the current shape into `buf` (`buf_len` ≥ width*height*4; size
/// `VGD_CURSOR_SHAPE_BUFFER_SIZE` always suffices). Returns `true` and
/// fills `out` on a consistent copy; `false` when no shape is published
/// yet or the driver was mid-rewrite (retry next frame).
#[no_mangle]
pub unsafe extern "C" fn vgd_cursor_shape(
    cursor: *mut VgdCursorHandle,
    buf: *mut u8,
    buf_len: u32,
    out: *mut VgdCursorShape,
) -> bool {
    if cursor.is_null() || buf.is_null() || out.is_null() {
        return false;
    }
    guarded(false, || {
        let slice = std::slice::from_raw_parts_mut(buf, buf_len as usize);
        match (*cursor).0.shape(slice) {
            Some(shape) => {
                *out = VgdCursorShape {
                    kind: shape.kind,
                    width: shape.width,
                    height: shape.height,
                    hotspot_x: shape.hotspot_x,
                    hotspot_y: shape.hotspot_y,
                    generation: shape.generation,
                };
                true
            }
            None => false,
        }
    })
}

/// Compose the named shared-texture name for (session, generation, slot)
/// into `out` (capacity 96 u16s, NUL-padded). Returns the char count.
/// Open with `ID3D11Device1::OpenSharedResourceByName`.
#[no_mangle]
pub unsafe extern "C" fn vgd_slot_texture_name(
    session_id: u64,
    generation: u32,
    slot: u32,
    out: *mut u16,
) -> u32 {
    if out.is_null() {
        return 0;
    }
    guarded(0, || {
        let mut buf = [0u16; 96];
        let len = names::slot_texture_name(session_id, generation, slot, &mut buf);
        std::ptr::copy_nonoverlapping(buf.as_ptr(), out, 96);
        len as u32
    })
}

/// Compose the D3D12-openable shared texture name for a claimed slot.
#[no_mangle]
pub unsafe extern "C" fn vgd_slot_texture_d3d12_name(
    session_id: u64,
    generation: u32,
    slot: u32,
    out: *mut u16,
) -> u32 {
    if out.is_null() {
        return 0;
    }
    guarded(0, || {
        let out = &mut *(out as *mut [u16; 96]);
        names::slot_texture_d3d12_name(session_id, generation, slot, out) as u32
    })
}

/// Compose the shared producer timeline-fence name for a ring generation.
#[no_mangle]
pub unsafe extern "C" fn vgd_ring_fence_name(
    session_id: u64,
    generation: u32,
    out: *mut u16,
) -> u32 {
    if out.is_null() {
        return 0;
    }
    guarded(0, || {
        let out = &mut *(out as *mut [u16; 96]);
        names::ring_fence_name(session_id, generation, out) as u32
    })
}

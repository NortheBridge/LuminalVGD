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
const _: () = assert!(VGD_CAP_HDR10 == luminal_driver_proto::caps::HDR10);
const _: () = assert!(VGD_CAP_SDR10_BIT == luminal_driver_proto::caps::SDR10_BIT);
const _: () = assert!(VGD_CAP_HW_CURSOR == luminal_driver_proto::caps::HW_CURSOR);

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
}

fn guarded<T>(default: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(default)
}

/// Open the LuminalVGD control device. NULL when the driver is absent —
/// the caller falls back to another backend.
#[no_mangle]
pub extern "C" fn vgd_device_open() -> *mut VgdDeviceHandle {
    guarded(null_mut(), || match VgdDevice::open_first() {
        Ok(dev) => Box::into_raw(Box::new(VgdDeviceHandle(dev))),
        Err(_) => null_mut(),
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
        Err(_) => VGD_ERR_IO,
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
            Err(_) => VGD_ERR_IO,
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
        Err(_) => null_mut(),
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

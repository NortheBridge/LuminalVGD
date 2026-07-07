// SPDX-License-Identifier: AGPL-3.0-only
//! luminal-driver-proto — the ONLY definition of the LuminalVGD host↔driver ABI.
//!
//! Rules (see docs/DESIGN.md §3.1 and CLAUDE.md):
//! - Both the driver and LuminalShine import this crate. Never redefine
//!   these types elsewhere.
//! - Breaking layout/semantic change => bump `PROTO_VERSION_MAJOR`.
//!   Additive change => bump `PROTO_VERSION_MINOR`.
//! - Everything shared across the process boundary is `#[repr(C)]`,
//!   explicitly sized, and free of pointers.

#![no_std]

/// Bump on breaking ABI changes. Host refuses to run on major mismatch.
pub const PROTO_VERSION_MAJOR: u16 = 0;
/// Bump on additive, backward-compatible changes.
pub const PROTO_VERSION_MINOR: u16 = 1;

/// Device interface GUID for the LuminalVGD control device.
/// {B3A7F2D4-6E1C-4A98-9D3B-5C0E8F714A26} — LuminalVGD-owned; do not reuse
/// pf-vdisplay's or SudoVDA's identifiers.
pub const LUMINAL_VGD_INTERFACE_GUID: (u32, u16, u16, [u8; 8]) = (
    0xB3A7_F2D4,
    0x6E1C,
    0x4A98,
    [0x9D, 0x3B, 0x5C, 0x0E, 0x8F, 0x71, 0x4A, 0x26],
);

/// Capability bits reported by the driver in the handshake.
pub mod caps {
    pub const HDR10: u32 = 1 << 0; // requires Win11 24H2 host support
    pub const HDR12_BIT: u32 = 1 << 1;
    pub const SDR10_BIT: u32 = 1 << 2;
    pub const DIRTY_RECTS: u32 = 1 << 3;
    pub const REFRESH_DOUBLING: u32 = 1 << 4; // frame-gen-aware modes
}

/// IOCTL function codes (combined with FILE_DEVICE_UNKNOWN / METHOD_BUFFERED
/// on the Windows side; kept as plain function numbers here).
pub mod ioctl {
    pub const HANDSHAKE: u32 = 0x800;
    pub const CREATE_MONITOR: u32 = 0x801;
    pub const DESTROY_MONITOR: u32 = 0x802;
    pub const PING: u32 = 0x803;
    pub const GET_STATUS: u32 = 0x804;
}

/// Defaults ported from SudoVDA semantics (docs/FEATURE-MATRIX.md).
pub const DEFAULT_MAX_MONITORS: u32 = 10;
/// Watchdog timeout in seconds; 0 disables. Driver destroys monitors whose
/// owner stops PINGing (host crash => no zombie displays).
pub const DEFAULT_WATCHDOG_SECS: u32 = 3;
/// Frame ring slot count (shared keyed-mutex textures).
pub const DEFAULT_RING_SLOTS: u32 = 3;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandshakeRequest {
    pub host_proto_major: u16,
    pub host_proto_minor: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandshakeReply {
    pub driver_proto_major: u16,
    pub driver_proto_minor: u16,
    pub driver_build: u32,
    pub caps: u32, // bitmask of `caps::*`
    pub max_monitors: u32,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitDepth {
    Sdr8 = 8,
    Sdr10 = 10,
    Hdr10 = 110,
    Hdr12 = 112,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CreateMonitorRequest {
    pub session_id: u64,
    pub width: u32,
    pub height: u32,
    pub refresh_millihz: u32, // 120000 = 120 Hz; millihertz avoids fractional-rate loss
    pub bit_depth: BitDepth,
    pub hdr: u32,             // 0/1; requires caps::HDR10
    /// Render adapter LUID; 0 => driver default (largest-VRAM adapter,
    /// SudoVDA-compatible behavior).
    pub adapter_luid: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CreateMonitorReply {
    pub session_id: u64,
    /// Name of the shared-memory section containing `RingHeader` + slots
    /// metadata, NUL-padded UTF-16LE.
    pub ring_section_name: [u16; 64],
    pub ring_slots: u32,
    pub result: i32, // 0 = ok; negative = proto error code
}

/// Header at offset 0 of the shared ring section. One writer (driver),
/// one reader (host). Slot texture handles are exchanged as NT shared
/// handles referenced by slot metadata that follows this header.
#[repr(C)]
pub struct RingHeader {
    /// Incremented by the driver whenever the D3D device/ring is rebuilt
    /// (TDR, adapter reset). Host re-maps handles when this changes.
    pub ring_generation: u32,
    pub slot_count: u32,
    /// Monotonic frame sequence of the most recently published slot.
    /// Gaps are legal (drop-oldest policy) and detectable by the host.
    pub latest_sequence: u64,
    /// QPC timestamp of the latest published frame.
    pub latest_present_qpc: u64,
}

#[cfg(test)]
mod layout_tests {
    // Phase 1 (CLAUDE.md): add static size/alignment assertions here before
    // any driver code consumes these types.
}

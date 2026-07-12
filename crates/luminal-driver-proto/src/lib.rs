// SPDX-License-Identifier: AGPL-3.0-only
//! luminal-driver-proto — the ONLY definition of the LuminalVGD host↔driver ABI.
//!
//! Rules (see docs/DESIGN.md §3.1 and CLAUDE.md):
//! - Both the driver and LuminalShine import this crate. Never redefine
//!   these types elsewhere.
//! - Breaking layout/semantic change => bump `PROTO_VERSION_MAJOR`.
//!   Additive change => bump `PROTO_VERSION_MINOR`. (Pre-1.0 exception:
//!   while `PROTO_VERSION_MAJOR == 0` nothing has shipped, so minor bumps
//!   may still re-layout; the handshake compares both numbers.)
//! - Everything shared across the process boundary is `#[repr(C)]`,
//!   explicitly sized, pointer-free, and enum-free (raw integer fields with
//!   checked conversion helpers, so a hostile or stale peer can never make
//!   an invalid Rust enum value materialize).
//! - Every struct's size and field offsets are locked by the assertions in
//!   `layout_tests` at the bottom of this file. A layout change that
//!   forgets the version bump fails to compile the moment the assertion is
//!   updated — update both together.

#![cfg_attr(not(test), no_std)]

use static_assertions::{const_assert, const_assert_eq};

// ---------------------------------------------------------------------------
// Protocol version
// ---------------------------------------------------------------------------

/// Bump on breaking ABI changes. Host refuses to run on major mismatch.
pub const PROTO_VERSION_MAJOR: u16 = 0;
/// Bump on additive, backward-compatible changes.
/// v0.3: display-identity/lease split, per-lease timeouts, multi-mode
/// monitors, physical dimensions, cursor section, permanent pool, render-
/// adapter IOCTL (libvirtualdisplay behavior fold-in; see
/// THIRD-PARTY-NOTICES.md).
pub const PROTO_VERSION_MINOR: u16 = 3;

/// Device interface GUID for the LuminalVGD control device.
/// {B3A7F2D4-6E1C-4A98-9D3B-5C0E8F714A26} — LuminalVGD-owned; do not reuse
/// pf-vdisplay's or SudoVDA's identifiers.
pub const LUMINAL_VGD_INTERFACE_GUID: (u32, u16, u16, [u8; 8]) = (
    0xB3A7_F2D4,
    0x6E1C,
    0x4A98,
    [0x9D, 0x3B, 0x5C, 0x0E, 0x8F, 0x71, 0x4A, 0x26],
);

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

/// Capability bits reported by the driver in the handshake.
pub mod caps {
    /// HDR10 output supported (requires Win11 24H2 host support).
    pub const HDR10: u32 = 1 << 0;
    /// 12-bit HDR pipeline supported.
    pub const HDR12_BIT: u32 = 1 << 1;
    /// 10-bit SDR supported.
    pub const SDR10_BIT: u32 = 1 << 2;
    /// Slot metadata carries meaningful dirty-rect summaries.
    pub const DIRTY_RECTS: u32 = 1 << 3;
    /// Driver honors frame-generation-aware doubled refresh modes.
    pub const REFRESH_DOUBLING: u32 = 1 << 4;
    /// Hardware cursor plane: driver fills the cursor section
    /// (`CursorHeader` + shape buffer) instead of compositing the cursor
    /// into frames, so the client can render it locally.
    pub const HW_CURSOR: u32 = 1 << 5;
    /// Gamma-ramp DDI supported (Night Light / calibration on the virtual
    /// display).
    pub const GAMMA_RAMP: u32 = 1 << 6;
    /// `CREATE_MONITOR` accepts up to `MAX_MODES_PER_MONITOR` modes.
    pub const MULTI_MODE: u32 = 1 << 7;
    /// Permanent display pool IOCTLs supported.
    pub const PERMANENT_POOL: u32 = 1 << 8;
}

// ---------------------------------------------------------------------------
// IOCTL codes
// ---------------------------------------------------------------------------

/// IOCTL definitions. `function` values are combined with
/// `FILE_DEVICE_UNKNOWN` / `METHOD_BUFFERED` / `FILE_ANY_ACCESS` exactly as
/// Windows' `CTL_CODE` macro does; use the `IOCTL_*` constants on both
/// sides so the encoded values can never diverge.
pub mod ioctl {
    const FILE_DEVICE_UNKNOWN: u32 = 0x0000_0022;
    const METHOD_BUFFERED: u32 = 0;
    const FILE_ANY_ACCESS: u32 = 0;

    /// Windows `CTL_CODE` encoding.
    pub const fn ctl_code(function: u32) -> u32 {
        (FILE_DEVICE_UNKNOWN << 16) | (FILE_ANY_ACCESS << 14) | (function << 2) | METHOD_BUFFERED
    }

    pub const FN_HANDSHAKE: u32 = 0x800;
    pub const FN_CREATE_MONITOR: u32 = 0x801;
    pub const FN_DESTROY_MONITOR: u32 = 0x802;
    pub const FN_PING: u32 = 0x803;
    pub const FN_GET_STATUS: u32 = 0x804;
    pub const FN_SET_RENDER_ADAPTER: u32 = 0x805;
    pub const FN_QUERY_LEASE: u32 = 0x806;
    pub const FN_SET_PERMANENT_POOL: u32 = 0x807;
    pub const FN_QUERY_PERMANENT_POOL: u32 = 0x808;

    /// In: [`HandshakeRequest`](super::HandshakeRequest), out: [`HandshakeReply`](super::HandshakeReply).
    pub const IOCTL_HANDSHAKE: u32 = ctl_code(FN_HANDSHAKE);
    /// In: [`CreateMonitorRequest`](super::CreateMonitorRequest), out: [`CreateMonitorReply`](super::CreateMonitorReply).
    pub const IOCTL_CREATE_MONITOR: u32 = ctl_code(FN_CREATE_MONITOR);
    /// In: [`DestroyMonitorRequest`](super::DestroyMonitorRequest), out: `i32` result.
    pub const IOCTL_DESTROY_MONITOR: u32 = ctl_code(FN_DESTROY_MONITOR);
    /// In: [`PingRequest`](super::PingRequest), out: `i32` result.
    pub const IOCTL_PING: u32 = ctl_code(FN_PING);
    /// In: none, out: [`GetStatusReply`](super::GetStatusReply).
    pub const IOCTL_GET_STATUS: u32 = ctl_code(FN_GET_STATUS);
    /// In: [`SetRenderAdapterRequest`](super::SetRenderAdapterRequest), out: `i32` result.
    /// Sets the device-wide preferred adapter used when a create request
    /// passes `adapter_luid == 0` (before falling back to largest-VRAM).
    pub const IOCTL_SET_RENDER_ADAPTER: u32 = ctl_code(FN_SET_RENDER_ADAPTER);
    /// In: [`QueryLeaseRequest`](super::QueryLeaseRequest), out: [`QueryLeaseReply`](super::QueryLeaseReply).
    pub const IOCTL_QUERY_LEASE: u32 = ctl_code(FN_QUERY_LEASE);
    /// In: [`PermanentPoolConfig`](super::PermanentPoolConfig), out: `i32` result.
    pub const IOCTL_SET_PERMANENT_POOL: u32 = ctl_code(FN_SET_PERMANENT_POOL);
    /// In: none, out: [`QueryPermanentPoolReply`](super::QueryPermanentPoolReply).
    pub const IOCTL_QUERY_PERMANENT_POOL: u32 = ctl_code(FN_QUERY_PERMANENT_POOL);
}

// ---------------------------------------------------------------------------
// Error codes
// ---------------------------------------------------------------------------

/// Result codes carried in `result` fields. `0` is success; negative values
/// are protocol errors. These are ABI — never renumber, only append.
pub mod err {
    pub const OK: i32 = 0;
    /// Handshake major version mismatch.
    pub const PROTO_MISMATCH: i32 = -1;
    /// Monitor cap (`max_monitors`) reached.
    pub const MAX_MONITORS: i32 = -2;
    /// Width/height/refresh outside the supported envelope.
    pub const BAD_MODE: i32 = -3;
    /// `bit_depth` is not one of the supported values.
    pub const BAD_BIT_DEPTH: i32 = -4;
    /// HDR requested but unsupported (caps or OS floor).
    pub const HDR_UNSUPPORTED: i32 = -5;
    /// `adapter_luid` does not name a usable render adapter.
    pub const NO_ADAPTER: i32 = -6;
    /// `CREATE_MONITOR` for a `session_id` that already has a monitor.
    pub const DUPLICATE_SESSION: i32 = -7;
    /// `DESTROY_MONITOR`/`PING` for an unknown `session_id`.
    pub const NO_SUCH_SESSION: i32 = -8;
    /// Shared ring section/texture allocation failed.
    pub const RING_ALLOC: i32 = -9;
    /// Handshake not completed on this handle before session IOCTLs.
    pub const NOT_HANDSHAKEN: i32 = -10;
    /// `display_id` is already bound to a live monitor.
    pub const IDENTITY_IN_USE: i32 = -11;
    /// Permanent-pool config invalid (count above cap, bad mode…).
    pub const BAD_POOL: i32 = -12;
    /// Unspecified driver-internal failure; details in `GET_STATUS`.
    pub const INTERNAL: i32 = -100;
}

// ---------------------------------------------------------------------------
// Defaults (SudoVDA-ported semantics, docs/FEATURE-MATRIX.md)
// ---------------------------------------------------------------------------

/// Default monitor cap. Registry-configurable, but never above
/// [`ABI_MAX_MONITORS`].
pub const DEFAULT_MAX_MONITORS: u32 = 10;
/// Hard ABI ceiling on monitors — sizes the `GET_STATUS` reply. The
/// effective cap is `min(configured, ABI_MAX_MONITORS)`.
pub const ABI_MAX_MONITORS: u32 = 16;
/// Watchdog timeout in seconds; 0 disables. Driver destroys monitors whose
/// owner stops PINGing (host crash => no zombie displays).
pub const DEFAULT_WATCHDOG_SECS: u32 = 3;
/// Frame ring slot count (shared keyed-mutex textures).
pub const DEFAULT_RING_SLOTS: u32 = 3;
/// Hard ABI ceiling on ring slots.
pub const ABI_MAX_RING_SLOTS: u32 = 8;

/// Maximum modes one monitor may advertise (libvirtualdisplay parity —
/// e.g. base + frame-gen-doubled refresh without a destroy/create cycle).
pub const MAX_MODES_PER_MONITOR: u32 = 4;
/// Maximum permanent (outside-any-stream) displays in the pool.
pub const MAX_PERMANENT_DISPLAYS: u32 = 4;

/// Lease (watchdog) timeout bounds, milliseconds. A request of
/// [`LEASE_TIMEOUT_USE_DEFAULT`] takes the driver's configured default;
/// [`LEASE_TIMEOUT_DISABLED`] disables reaping for that monitor
/// (permanent displays use this); anything else is clamped to
/// [`MIN_LEASE_TIMEOUT_MS`]..=[`MAX_LEASE_TIMEOUT_MS`].
pub const DEFAULT_LEASE_TIMEOUT_MS: u32 = 10_000;
pub const MIN_LEASE_TIMEOUT_MS: u32 = 3_000;
pub const MAX_LEASE_TIMEOUT_MS: u32 = 300_000;
pub const LEASE_TIMEOUT_USE_DEFAULT: u32 = 0;
pub const LEASE_TIMEOUT_DISABLED: u32 = u32::MAX;

/// Keyed-mutex key the driver holds while writing a slot.
pub const KMTX_KEY_DRIVER: u64 = 0;
/// Keyed-mutex key the host acquires to read a published slot.
pub const KMTX_KEY_HOST: u64 = 1;
/// Bounded wait for every keyed-mutex acquire, both sides (DESIGN.md §3.3
/// rule 1: no unbounded waits anywhere). Milliseconds.
pub const KMTX_ACQUIRE_TIMEOUT_MS: u32 = 100;

// ---------------------------------------------------------------------------
// Bit depth
// ---------------------------------------------------------------------------

/// Supported bit-depth / dynamic-range combinations (SudoVDA-ported set).
/// Carried on the wire as a raw `u32` — use [`BitDepth::from_raw`].
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BitDepth {
    Sdr8 = 8,
    Sdr10 = 10,
    Hdr10 = 110,
    Hdr12 = 112,
}

impl BitDepth {
    /// Checked conversion from the wire value.
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            8 => Some(Self::Sdr8),
            10 => Some(Self::Sdr10),
            110 => Some(Self::Hdr10),
            112 => Some(Self::Hdr12),
            _ => None,
        }
    }

    pub const fn as_raw(self) -> u32 {
        self as u32
    }

    pub const fn is_hdr(self) -> bool {
        matches!(self, Self::Hdr10 | Self::Hdr12)
    }
}

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

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
    /// Monotonic driver build number (CI-stamped).
    pub driver_build: u32,
    /// Bitmask of `caps::*`.
    pub caps: u32,
    /// Effective monitor cap (`min(configured, ABI_MAX_MONITORS)`).
    pub max_monitors: u32,
    /// Effective watchdog timeout in seconds (0 = disabled).
    pub watchdog_secs: u32,
}

/// The compatibility rule, defined once for both sides: same major, and the
/// driver's minor must be at least the host's (the host only depends on
/// features that existed when it was built).
pub const fn versions_compatible(
    host_major: u16,
    host_minor: u16,
    driver_major: u16,
    driver_minor: u16,
) -> bool {
    host_major == driver_major && driver_minor >= host_minor
}

// ---------------------------------------------------------------------------
// Monitor lifecycle
// ---------------------------------------------------------------------------

/// `CreateMonitorRequest.flags` bits.
pub mod create_flags {
    /// Informational: the host doubled the client's refresh rate because
    /// frame generation is active (policy is host-side; the driver just
    /// honors the mode — DESIGN.md §5).
    pub const REFRESH_DOUBLED: u32 = 1 << 0;
    /// Ignore `display_id` and mint a throwaway identity: Windows will not
    /// associate this monitor with any remembered display settings
    /// (libvirtualdisplay's ephemeral-identity behavior).
    pub const EPHEMERAL_IDENTITY: u32 = 1 << 1;
}

/// One display mode. `modes[0]` is the preferred/native mode.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModeSpec {
    pub width: u32,
    pub height: u32,
    /// 120000 = 120 Hz; millihertz avoids fractional-rate loss (59.94 etc.).
    pub refresh_millihz: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CreateMonitorRequest {
    /// Host-chosen unique id for this *lease* (stream lifetime); keys every
    /// later `PING`/`DESTROY_MONITOR`/`QUERY_LEASE` and the shared-section
    /// names.
    pub session_id: u64,
    /// Stable display *identity*, independent of the lease: reconnecting
    /// with the same `display_id` reclaims the same connector, EDID product
    /// code, and serial, so Windows recognizes the monitor and keeps its
    /// settings (resolution/HDR/position). 0 => derive an ephemeral
    /// identity from `session_id` (same effect as `EPHEMERAL_IDENTITY`).
    pub display_id: u64,
    /// Render adapter LUID; 0 => device preference set via
    /// `SET_RENDER_ADAPTER`, else largest-VRAM (SudoVDA-compatible).
    pub adapter_luid: u64,
    /// See the `LEASE_TIMEOUT_*` constants.
    pub lease_timeout_ms: u32,
    /// Raw [`BitDepth`] wire value — validate with `BitDepth::from_raw`.
    /// Applies to all modes.
    pub bit_depth: u32,
    /// 0/1; requires `caps::HDR10` and an HDR-capable `bit_depth`.
    pub hdr: u32,
    /// EDID serial override; 0 => derived from the display identity
    /// (recommended — keeps identity retention coherent).
    pub edid_serial: u32,
    /// Bitmask of `create_flags::*`.
    pub flags: u32,
    /// Number of valid entries in `modes` (1..=`MAX_MODES_PER_MONITOR`).
    pub mode_count: u32,
    pub modes: [ModeSpec; MAX_MODES_PER_MONITOR as usize],
    /// Physical panel size advertised in the EDID, millimeters — drives
    /// Windows DPI scaling. 0 => defaults (600×340, ≈27" 16:9).
    pub physical_width_mm: u32,
    pub physical_height_mm: u32,
    /// Monitor friendly name for the EDID descriptor, NUL-padded UTF-16LE
    /// (truncated to 13 chars by EDID rules; longer is fine here).
    pub friendly_name: [u16; 32],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CreateMonitorReply {
    pub session_id: u64,
    /// The effective identity (echoed, or the derived ephemeral one).
    pub display_id: u64,
    /// `err::OK` or a negative `err::*` code. On error every other field
    /// except `session_id` is zero.
    pub result: i32,
    /// Number of slots in the ring (≤ `ABI_MAX_RING_SLOTS`).
    pub ring_slots: u32,
    /// IddCx connector this identity is (re-)attached to.
    pub connector_index: u32,
    pub reserved: u32,
    /// Name of the shared-memory section containing [`RingHeader`] +
    /// [`SlotMetadata`] array, NUL-padded UTF-16LE. Composed with
    /// [`names::ring_section_name`].
    pub ring_section_name: [u16; 64],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DestroyMonitorRequest {
    pub session_id: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PingRequest {
    pub session_id: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SetRenderAdapterRequest {
    /// Device-wide preferred adapter for `adapter_luid == 0` creates;
    /// 0 clears the preference (back to largest-VRAM default).
    pub adapter_luid: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryLeaseRequest {
    pub session_id: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryLeaseReply {
    pub session_id: u64,
    pub display_id: u64,
    /// Milliseconds until the watchdog reaps this monitor if no PING
    /// arrives; `u32::MAX` when the lease never expires.
    pub remaining_ms: u32,
    pub connector_index: u32,
    pub result: i32,
    pub reserved: u32,
}

/// Permanent-display pool configuration: `count` identical always-on
/// displays that exist outside any streaming session and survive driver
/// restarts (libvirtualdisplay's permanent pool; the modern replacement
/// for SudoVDA's `option.txt`). Serves LuminalShine's
/// keep-display-while-paused behavior.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PermanentPoolConfig {
    /// 0..=`MAX_PERMANENT_DISPLAYS`; 0 disbands the pool.
    pub count: u32,
    pub width: u32,
    pub height: u32,
    pub refresh_millihz: u32,
    pub bit_depth: u32,
    pub hdr: u32,
    pub physical_width_mm: u32,
    pub physical_height_mm: u32,
    /// NUL-padded UTF-16LE.
    pub name: [u16; 32],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryPermanentPoolReply {
    pub config: PermanentPoolConfig,
    pub result: i32,
    pub reserved: u32,
}

// ---------------------------------------------------------------------------
// Shared ring
// ---------------------------------------------------------------------------

/// `RingHeader.state` values. 0 is deliberately unused so an all-zero
/// (never-initialized) mapping is detectable.
pub mod ring_state {
    /// Ring is live; driver is publishing frames.
    pub const ACTIVE: u32 = 1;
    /// Driver detected device removal/TDR and is rebuilding the D3D device
    /// and textures. `ring_generation` will increment when done. The host
    /// should fall back (seamlessly) and poll for the generation bump.
    pub const REBUILDING: u32 = 2;
    /// Ring is permanently dead (monitor destroyed). Unmap and stop.
    pub const DEAD: u32 = 3;
}

/// `SlotMetadata.state` values. Written with release ordering by whichever
/// side owns the transition, read with acquire ordering.
pub mod slot_state {
    /// Free for the driver to write.
    pub const FREE: u32 = 0;
    /// Driver is copying into the slot texture.
    pub const WRITING: u32 = 1;
    /// Frame complete; available to the host.
    pub const PUBLISHED: u32 = 2;
    /// Host holds the slot (keyed mutex acquired).
    pub const READING: u32 = 3;
}

/// `SlotMetadata.flags` bits.
pub mod slot_flags {
    /// The `hdr` metadata block in this slot is valid.
    pub const HDR_METADATA_VALID: u32 = 1 << 0;
    /// The dirty-rect summary in this slot is valid (else assume full-frame).
    pub const DIRTY_RECTS_VALID: u32 = 1 << 1;
}

/// Exact mirror of `DXGI_HDR_METADATA_HDR10` (CTA-861.3 static metadata):
/// primaries/white point in 0.00002 units, luminance in 0.0001 nit units
/// (max) / 0.0001 nit units (min), light levels in nits.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Hdr10StaticMetadata {
    pub red_primary: [u16; 2],
    pub green_primary: [u16; 2],
    pub blue_primary: [u16; 2],
    pub white_point: [u16; 2],
    pub max_mastering_luminance: u32,
    pub min_mastering_luminance: u32,
    pub max_content_light_level: u16,
    pub max_frame_average_light_level: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RectU32 {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

/// Per-slot metadata, laid out as an array immediately after the header
/// (at [`RING_SLOTS_OFFSET`]). The slot's pixel data lives in a named
/// shared D3D texture (see [`names::slot_texture_name`]), not in the
/// section — the section carries only control data.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SlotMetadata {
    /// One of `slot_state::*`. Atomic access only.
    pub state: u32,
    /// Bitmask of `slot_flags::*`.
    pub flags: u32,
    /// Monotonic frame sequence number of the frame in this slot.
    pub sequence: u64,
    /// QPC timestamp when the frame was presented to the driver.
    pub present_qpc: u64,
    pub hdr: Hdr10StaticMetadata,
    /// Number of dirty rects the compositor reported (0 = unknown).
    pub dirty_count: u32,
    /// Bounding box of all dirty rects (valid if `DIRTY_RECTS_VALID`).
    pub dirty_bound: RectU32,
    pub reserved: [u32; 2],
}

/// Header at offset 0 of the shared ring section. One writer (driver),
/// one reader (host).
#[repr(C)]
pub struct RingHeader {
    /// Always [`RING_MAGIC`] once the driver has initialized the section.
    pub magic: u32,
    /// Layout version of the section contents; bump with `PROTO_VERSION`.
    pub header_version: u32,
    /// Incremented by the driver whenever the D3D device/ring is rebuilt
    /// (TDR, adapter reset). Slot texture names embed the generation, so
    /// the host re-opens textures when this changes (DESIGN.md §3.3).
    pub ring_generation: u32,
    pub slot_count: u32,
    /// One of `ring_state::*`. The host's fallback/restore logic keys off
    /// this plus `driver_heartbeat_qpc`.
    pub state: u32,
    pub reserved0: u32,
    /// Monotonic frame sequence of the most recently published slot.
    /// Gaps are legal (drop-oldest policy) and detectable by the host.
    pub latest_sequence: u64,
    /// QPC timestamp of the latest published frame.
    pub latest_present_qpc: u64,
    /// Total frames ever published on this ring.
    pub frames_published: u64,
    /// Frames dropped because no slot was free (host stalled).
    pub frames_dropped: u64,
    /// Updated by the driver at least every 500 ms even when idle; a stale
    /// heartbeat tells the host "driver gone/wedged" (escalate) vs. a
    /// `REBUILDING` state with fresh heartbeat ("wait for generation bump").
    pub driver_heartbeat_qpc: u64,
    /// QueryPerformanceFrequency of the driver's QPC domain, so the host
    /// can convert QPC deltas without a second syscall contract.
    pub qpc_frequency: u64,
}

pub const RING_MAGIC: u32 = 0x4C56_4752; // "RGVL" little-endian => "LVGR"
/// Version of the ring-section layout (header + slot array).
pub const RING_HEADER_VERSION: u32 = 1;
/// Slot metadata array starts at this offset (header padded to a cache
/// line so header churn and slot churn don't false-share).
pub const RING_SLOTS_OFFSET: usize = 128;
/// Driver must refresh `driver_heartbeat_qpc` at least this often (ms).
pub const RING_HEARTBEAT_INTERVAL_MS: u32 = 500;
/// Host treats the driver as wedged when the heartbeat is older than this.
pub const RING_HEARTBEAT_STALE_MS: u32 = 2000;

/// Total byte size of a ring section for `slots` slots.
pub const fn ring_section_size(slots: u32) -> usize {
    RING_SLOTS_OFFSET + (slots as usize) * core::mem::size_of::<SlotMetadata>()
}

// ---------------------------------------------------------------------------
// Cursor section (caps::HW_CURSOR)
// ---------------------------------------------------------------------------

/// `CursorHeader.kind` values (mirrors IddCx cursor shape types).
pub mod cursor_kind {
    /// 32bpp premultiplied-alpha ARGB.
    pub const ALPHA: u32 = 1;
    /// 32bpp color, no alpha.
    pub const COLOR: u32 = 2;
    /// Monochrome AND/XOR masked.
    pub const MASKED: u32 = 3;
}

/// Maximum cursor shape dimension (matches the IddCx hardware-cursor cap
/// libvirtualdisplay ships).
pub const CURSOR_MAX_DIM: u32 = 256;
pub const CURSOR_MAGIC: u32 = 0x4C56_4743; // "CGVL" LE => "LVGC"
pub const CURSOR_HEADER_VERSION: u32 = 1;
/// Shape pixel buffer starts at this offset in the cursor section.
pub const CURSOR_SHAPE_OFFSET: usize = 64;

/// Header at offset 0 of the per-monitor cursor section. One writer
/// (driver, fed by IddCx cursor callbacks), one reader (host). Position
/// updates only touch `x`/`y`/`visible`/`position_qpc`; shape updates
/// rewrite the buffer then bump `shape_generation` (host re-reads the
/// shape when the generation changes — read generation, copy, re-check).
#[repr(C)]
pub struct CursorHeader {
    pub magic: u32,
    pub version: u32,
    /// Incremented after each complete shape-buffer rewrite.
    pub shape_generation: u32,
    pub width: u32,
    pub height: u32,
    pub hotspot_x: u32,
    pub hotspot_y: u32,
    /// One of `cursor_kind::*`.
    pub kind: u32,
    /// Desktop coordinates on the virtual display.
    pub x: i32,
    pub y: i32,
    pub visible: u32,
    pub reserved0: u32,
    pub position_qpc: u64,
    pub reserved1: u64,
}

/// Total cursor section size: header + worst-case 32bpp shape.
pub const fn cursor_section_size() -> usize {
    CURSOR_SHAPE_OFFSET + (CURSOR_MAX_DIM as usize) * (CURSOR_MAX_DIM as usize) * 4
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct MonitorStatus {
    pub session_id: u64,
    pub display_id: u64,
    pub adapter_luid: u64,
    pub latest_sequence: u64,
    pub frames_published: u64,
    pub frames_dropped: u64,
    /// Driver-clock milliseconds of the last `PING` for this session
    /// (watchdog input; same clock as `GetStatusReply.uptime_ms`).
    pub last_ping_ms: u64,
    /// Preferred (first) mode; the full list is create-time state.
    pub width: u32,
    pub height: u32,
    pub refresh_millihz: u32,
    pub bit_depth: u32,
    pub hdr: u32,
    pub ring_generation: u32,
    /// One of `ring_state::*`.
    pub ring_state: u32,
    /// Last `err::*` recorded for this monitor (sticky until destroy).
    pub last_error: i32,
    pub connector_index: u32,
    /// Effective lease timeout (`u32::MAX` = never expires).
    pub lease_timeout_ms: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct GetStatusReply {
    pub uptime_ms: u64,
    pub driver_build: u32,
    pub proto_major: u16,
    pub proto_minor: u16,
    pub caps: u32,
    pub max_monitors: u32,
    pub watchdog_secs: u32,
    /// Number of valid entries in `monitors`.
    pub monitor_count: u32,
    pub monitors: [MonitorStatus; ABI_MAX_MONITORS as usize],
}

// ---------------------------------------------------------------------------
// Kernel-namespace object names
// ---------------------------------------------------------------------------

/// Shared-object naming scheme. Both sides derive names from
/// (`session_id`, `ring_generation`, slot) with these helpers — never
/// hand-format them.
pub mod names {
    /// Writes ASCII `text` into `out` as UTF-16, returns chars written.
    fn put(out: &mut [u16], at: usize, text: &str) -> usize {
        let mut i = at;
        for b in text.bytes() {
            out[i] = b as u16;
            i += 1;
        }
        i
    }

    /// Writes `value` as fixed-width lowercase hex, returns next index.
    fn put_hex(out: &mut [u16], at: usize, value: u64, digits: usize) -> usize {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for d in 0..digits {
            let shift = (digits - 1 - d) * 4;
            out[at + d] = HEX[((value >> shift) & 0xF) as usize] as u16;
        }
        at + digits
    }

    /// Ring control section: `Global\LuminalVGD-ring-<session_id:016x>`.
    /// Returns the number of valid chars; the rest of `out` is NUL-padded.
    pub fn ring_section_name(session_id: u64, out: &mut [u16; 64]) -> usize {
        out.fill(0);
        let i = put(out, 0, "Global\\LuminalVGD-ring-");
        put_hex(out, i, session_id, 16)
    }

    /// Cursor section: `Global\LuminalVGD-cur-<session_id:016x>`.
    pub fn cursor_section_name(session_id: u64, out: &mut [u16; 64]) -> usize {
        out.fill(0);
        let i = put(out, 0, "Global\\LuminalVGD-cur-");
        put_hex(out, i, session_id, 16)
    }

    /// Slot texture shared handle:
    /// `Global\LuminalVGD-tex-<session_id:016x>-g<generation:08x>-s<slot:02x>`.
    /// Generation is baked into the name so a rebuilt ring can never alias
    /// a stale handle. Returns the number of valid chars.
    pub fn slot_texture_name(
        session_id: u64,
        generation: u32,
        slot: u32,
        out: &mut [u16; 96],
    ) -> usize {
        out.fill(0);
        let mut i = put(out, 0, "Global\\LuminalVGD-tex-");
        i = put_hex(out, i, session_id, 16);
        i = put(out, i, "-g");
        i = put_hex(out, i, generation as u64, 8);
        i = put(out, i, "-s");
        put_hex(out, i, slot as u64, 2)
    }
}

// ---------------------------------------------------------------------------
// Layout lock (Phase 1, CLAUDE.md): compile-time size/alignment assertions.
// If one of these fires, you changed the ABI — bump PROTO_VERSION and fix
// the assertion in the same commit.
// ---------------------------------------------------------------------------

mod layout_tests {
    use super::*;
    use core::mem::{align_of, size_of};

    const_assert_eq!(size_of::<HandshakeRequest>(), 4);
    const_assert_eq!(align_of::<HandshakeRequest>(), 2);

    const_assert_eq!(size_of::<HandshakeReply>(), 20);
    const_assert_eq!(align_of::<HandshakeReply>(), 4);

    const_assert_eq!(size_of::<ModeSpec>(), 12);

    const_assert_eq!(size_of::<CreateMonitorRequest>(), 168);
    const_assert_eq!(align_of::<CreateMonitorRequest>(), 8);

    const_assert_eq!(size_of::<CreateMonitorReply>(), 160);
    const_assert_eq!(align_of::<CreateMonitorReply>(), 8);

    const_assert_eq!(size_of::<DestroyMonitorRequest>(), 8);
    const_assert_eq!(size_of::<PingRequest>(), 8);
    const_assert_eq!(size_of::<SetRenderAdapterRequest>(), 8);
    const_assert_eq!(size_of::<QueryLeaseRequest>(), 8);
    const_assert_eq!(size_of::<QueryLeaseReply>(), 32);
    const_assert_eq!(align_of::<QueryLeaseReply>(), 8);

    const_assert_eq!(size_of::<PermanentPoolConfig>(), 96);
    const_assert_eq!(size_of::<QueryPermanentPoolReply>(), 104);

    const_assert_eq!(size_of::<CursorHeader>(), 64);
    const_assert_eq!(align_of::<CursorHeader>(), 8);
    const_assert_eq!(cursor_section_size(), 64 + 256 * 256 * 4);

    const_assert_eq!(size_of::<Hdr10StaticMetadata>(), 28);
    const_assert_eq!(align_of::<Hdr10StaticMetadata>(), 4);

    const_assert_eq!(size_of::<RectU32>(), 16);

    const_assert_eq!(size_of::<SlotMetadata>(), 80);
    const_assert_eq!(align_of::<SlotMetadata>(), 8);

    const_assert_eq!(size_of::<RingHeader>(), 72);
    const_assert_eq!(align_of::<RingHeader>(), 8);
    // Header must fit below the slot array.
    const_assert!(size_of::<RingHeader>() <= RING_SLOTS_OFFSET);

    const_assert_eq!(size_of::<MonitorStatus>(), 96);
    const_assert_eq!(align_of::<MonitorStatus>(), 8);

    const_assert_eq!(
        size_of::<GetStatusReply>(),
        32 + 96 * ABI_MAX_MONITORS as usize
    );
    const_assert_eq!(align_of::<GetStatusReply>(), 8);

    // IOCTL codes are ABI: lock the encoded values, not just the function
    // numbers.
    const_assert_eq!(ioctl::IOCTL_HANDSHAKE, 0x0022_2000);
    const_assert_eq!(ioctl::IOCTL_CREATE_MONITOR, 0x0022_2004);
    const_assert_eq!(ioctl::IOCTL_DESTROY_MONITOR, 0x0022_2008);
    const_assert_eq!(ioctl::IOCTL_PING, 0x0022_200C);
    const_assert_eq!(ioctl::IOCTL_GET_STATUS, 0x0022_2010);
    const_assert_eq!(ioctl::IOCTL_SET_RENDER_ADAPTER, 0x0022_2014);
    const_assert_eq!(ioctl::IOCTL_QUERY_LEASE, 0x0022_2018);
    const_assert_eq!(ioctl::IOCTL_SET_PERMANENT_POOL, 0x0022_201C);
    const_assert_eq!(ioctl::IOCTL_QUERY_PERMANENT_POOL, 0x0022_2020);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16_str(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16(&buf[..end]).unwrap()
    }

    #[test]
    fn bit_depth_round_trips_and_rejects_junk() {
        for d in [BitDepth::Sdr8, BitDepth::Sdr10, BitDepth::Hdr10, BitDepth::Hdr12] {
            assert_eq!(BitDepth::from_raw(d.as_raw()), Some(d));
        }
        assert_eq!(BitDepth::from_raw(0), None);
        assert_eq!(BitDepth::from_raw(12), None);
        assert_eq!(BitDepth::from_raw(u32::MAX), None);
        assert!(BitDepth::Hdr10.is_hdr());
        assert!(BitDepth::Hdr12.is_hdr());
        assert!(!BitDepth::Sdr8.is_hdr());
        assert!(!BitDepth::Sdr10.is_hdr());
    }

    #[test]
    fn version_compat_rule() {
        // Same major, driver minor >= host minor: ok.
        assert!(versions_compatible(0, 2, 0, 2));
        assert!(versions_compatible(0, 1, 0, 2));
        // Driver older than host: refuse.
        assert!(!versions_compatible(0, 2, 0, 1));
        // Major mismatch: refuse both directions.
        assert!(!versions_compatible(1, 0, 0, 9));
        assert!(!versions_compatible(0, 9, 1, 0));
    }

    #[test]
    fn ring_section_size_matches_layout() {
        assert_eq!(ring_section_size(0), 128);
        assert_eq!(ring_section_size(3), 128 + 3 * 80);
        assert_eq!(
            ring_section_size(ABI_MAX_RING_SLOTS),
            128 + 8 * 80
        );
    }

    #[test]
    fn ring_section_name_is_deterministic() {
        let mut a = [0u16; 64];
        let mut b = [0u16; 64];
        let la = names::ring_section_name(0xDEAD_BEEF_0000_0001, &mut a);
        let lb = names::ring_section_name(0xDEAD_BEEF_0000_0001, &mut b);
        assert_eq!(a, b);
        assert_eq!(la, lb);
        assert_eq!(
            utf16_str(&a),
            "Global\\LuminalVGD-ring-deadbeef00000001"
        );
        // NUL padding after the name.
        assert!(a[la..].iter().all(|&c| c == 0));
    }

    #[test]
    fn slot_texture_name_embeds_generation_and_slot() {
        let mut n = [0u16; 96];
        let len = names::slot_texture_name(0x0000_0000_0000_00AB, 7, 2, &mut n);
        assert_eq!(
            utf16_str(&n),
            "Global\\LuminalVGD-tex-00000000000000ab-g00000007-s02"
        );
        assert_eq!(len, utf16_str(&n).chars().count());

        // Different generation => different name (stale-handle aliasing is
        // structurally impossible).
        let mut n2 = [0u16; 96];
        names::slot_texture_name(0x0000_0000_0000_00AB, 8, 2, &mut n2);
        assert_ne!(n, n2);
    }

    #[test]
    fn cursor_section_name_is_distinct_from_ring() {
        let mut ring = [0u16; 64];
        let mut cur = [0u16; 64];
        names::ring_section_name(0xAB, &mut ring);
        names::cursor_section_name(0xAB, &mut cur);
        assert_ne!(ring, cur);
        assert_eq!(utf16_str(&cur), "Global\\LuminalVGD-cur-00000000000000ab");
    }

    #[test]
    fn guid_matches_documented_value() {
        let (a, b, c, d) = LUMINAL_VGD_INTERFACE_GUID;
        assert_eq!(a, 0xB3A7F2D4);
        assert_eq!(b, 0x6E1C);
        assert_eq!(c, 0x4A98);
        assert_eq!(d, [0x9D, 0x3B, 0x5C, 0x0E, 0x8F, 0x71, 0x4A, 0x26]);
    }
}

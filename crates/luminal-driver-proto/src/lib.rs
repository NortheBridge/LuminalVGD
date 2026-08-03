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
/// v0.4: `CreateMonitorRequest.max_nits` (host-configurable HDR peak
/// luminance for the EDID CTA-861.3 block; 0 = the 0.3 default). Purely
/// additive: the driver accepts the legacy 0.3 request size (missing
/// tail reads as zeros), so hosts should announce
/// [`PROTO_VERSION_MINOR_REQUIRED`] at handshake — not this constant —
/// unless they genuinely refuse to run without 0.4 features.
/// v0.5: `UPDATE_MODES` (`FN_UPDATE_MODES`) — change which of a LIVE
/// monitor's create-time modes it currently offers, without a
/// DESTROY+CREATE cycle, plus
/// [`caps::DYNAMIC_MODES`], [`UpdateModesRequest`]/[`UpdateModesReply`]
/// and [`err::UPDATE_FAILED`]. Purely additive: no existing struct,
/// IOCTL value, or error code moved, so a 0.3/0.4 host that never sends
/// the new opcode is unaffected. Hosts detect the capability with the
/// caps bit (see its docs) and fall back to the create-time mode list.
/// v0.6: Build 20's failed-swapchain retirement contract. A driver that
/// reports this version explicitly deletes an unrecoverable IDDCX_SWAPCHAIN
/// WDF object and accepts a fresh assignment without departing the monitor.
/// No wire layout changes; the minor identifies stronger recovery semantics.
/// v0.7: Build 21's explicitly-synchronised D3D12 transport. Hosts opt in
/// per monitor with [`create_flags::D3D12_FENCE_TRANSPORT`] after checking
/// [`caps::D3D12_FENCE_TRANSPORT`]. The existing named D3D11/keyed-mutex
/// transport remains the default and its ABI is unchanged.
/// v0.8: Build 22's first-frame admission contract. Ring state remains
/// REBUILDING after SetDevice and becomes ACTIVE only after a copied,
/// synchronized slot is published. No layout changes.
/// v0.9: Build 23 publishes the transport actually selected by the driver
/// in [`RingHeader::transport_flags`]. A requested D3D12-fence transport
/// can therefore downgrade in place to the keyed-mutex transport without
/// cycling the monitor.
pub const PROTO_VERSION_MINOR: u16 = 9;

/// The minimum driver minor a host actually REQUIRES. Hosts that degrade
/// gracefully when 0.4 fields are ignored (the nits value simply stays at
/// the driver default) announce this at handshake so 0.3 drivers keep
/// working; `versions_compatible` treats the announced value as the
/// host's floor.
pub const PROTO_VERSION_MINOR_REQUIRED: u16 = 3;

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
    /// `UPDATE_MODES` (proto 0.5) is implemented: which of a LIVE
    /// monitor's create-time modes it offers can be changed without a
    /// destroy/create cycle. (It does NOT mean new modes can be added —
    /// see [`UpdateModesRequest`](super::UpdateModesRequest); no IddCx DDI
    /// can do that on an arrived monitor.)
    ///
    /// THE detection mechanism for the feature — preferred over comparing
    /// `driver_proto_minor >= 5`, because caps travel in both the
    /// handshake reply and `GET_STATUS`, cross the FFI in `VgdCaps.caps`,
    /// and (unlike a monotonic version number) can be CLEARED by a driver
    /// that has the opcode compiled in but had to disable the capability.
    /// A host that sends `UPDATE_MODES` without this bit set is not
    /// broken — an older driver answers `STATUS_INVALID_DEVICE_REQUEST`,
    /// never a false success — but it wastes a round trip and logs
    /// nothing useful.
    ///
    /// Deliberately NOT [`REFRESH_DOUBLING`] (1 << 4): that bit is
    /// defined, has never been set by any shipped driver, and means
    /// something else. Repurposing a shipped ABI constant is how a
    /// capability check silently starts lying.
    pub const DYNAMIC_MODES: u32 = 1 << 9;
    /// The driver can publish D3D12-openable named textures plus an
    /// ID3D11Fence/ID3D12Fence-compatible shared timeline. This is opt-in per
    /// monitor; older hosts continue receiving keyed-mutex textures.
    pub const D3D12_FENCE_TRANSPORT: u32 = 1 << 10;
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
    /// Proto 0.5. Appended — the nine below it keep their function
    /// numbers and their encoded IOCTL values forever.
    pub const FN_UPDATE_MODES: u32 = 0x809;

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
    /// In: [`UpdateModesRequest`](super::UpdateModesRequest), out:
    /// [`UpdateModesReply`](super::UpdateModesReply). Proto 0.5, gated on
    /// [`caps::DYNAMIC_MODES`](super::caps::DYNAMIC_MODES).
    ///
    /// Replaces the target-mode list a LIVE monitor publishes, within its
    /// create-time superset (see
    /// [`UpdateModesRequest`](super::UpdateModesRequest) for the
    /// replace-target-modes semantics). An older driver answers
    /// `STATUS_INVALID_DEVICE_REQUEST` for this code, which surfaces as an
    /// I/O failure host-side — never as a false success.
    pub const IOCTL_UPDATE_MODES: u32 = ctl_code(FN_UPDATE_MODES);
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
    /// `UPDATE_MODES`: the published target list could NOT be changed and
    /// the PREVIOUS list is still in force. This is the constraint-1
    /// degrade signal — never a departed monitor, never a refused session.
    ///
    /// **Retryable.** It means "not applied, for a reason that may not
    /// apply next time" — the OS refused the call, a TDR duck was in
    /// flight, the adapter was being torn down. Resending the identical
    /// request really does push again. The one non-retryable refusal has
    /// its own code, [`MODE_COMMITTED`], precisely so a retry loop can
    /// tell the two apart.
    ///
    /// Note where it can appear: the `UPDATE_MODES` reply completes before
    /// the driver calls the OS, so the reply carries driver-side
    /// validation only. This code surfaces afterwards, as the affected
    /// monitor's sticky `MonitorStatus.last_error` in `GET_STATUS`
    /// (plus an ETW event with stage and status).
    pub const UPDATE_FAILED: i32 = -13;
    /// `UPDATE_MODES`: the requested target list would have gated out the
    /// mode the OS has COMMITTED on that monitor, so the push was refused
    /// and the previously published list is still in force.
    ///
    /// **NOT retryable while that mode stays committed.** Publishing a
    /// subset without the committed mode forces Windows to re-select on a
    /// display that — under an exclusive virtual-display layout — is the
    /// only active one, i.e. a modeset in the middle of the stream. The
    /// driver refuses the PUSH (never the session, never the monitor), and
    /// resending the same list produces the same refusal every time.
    ///
    /// A host that gets this has two ways forward, and retrying is neither:
    ///
    /// 1. publish a subset that KEEPS the committed mode — read
    ///    [`UpdateModesReply::blocking_mode_idx`], which indexes the mode
    ///    list the monitor was CREATED with, to find out which one that is;
    ///    or
    /// 2. move the display onto the mode it wants FIRST (`SetDisplayConfig`
    ///    — the modeset is then the host's own, deliberate one), and gate
    ///    afterwards.
    ///
    /// Appears both as the `UPDATE_MODES` reply `result` (alongside
    /// [`update_status::BLOCKED`], on a request the driver can answer from
    /// the refusal it already made) and as the monitor's sticky
    /// `MonitorStatus.last_error` in `GET_STATUS` after the push it
    /// refused.
    pub const MODE_COMMITTED: i32 = -14;
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
///
/// 10, not the SudoVDA-inherited 3: `effective_lease_timeout` floors
/// USE_DEFAULT leases at `DEFAULT_LEASE_TIMEOUT_MS` (10 s) regardless,
/// so 3 was never honored — the handshake advertised "watchdog 3 s"
/// while orphaned monitors actually lived ~10-11 s (measured in the
/// 2026-07-27 field incident). The advertised value now matches the
/// enforced one.
pub const DEFAULT_WATCHDOG_SECS: u32 = 10;
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
    /// Request D3D12-openable ring textures and explicit timeline-fence
    /// synchronisation instead of the legacy keyed-mutex texture contract.
    /// Valid only when the driver advertises `caps::D3D12_FENCE_TRANSPORT`.
    pub const D3D12_FENCE_TRANSPORT: u32 = 1 << 2;
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
    /// Desired HDR peak luminance ("max nits") for the EDID's CTA-861.3
    /// block. 0 => driver default (≈993 nits — the pre-0.4 behavior, and
    /// what a zero-initialized or legacy 0.3-sized request yields).
    /// Quantized to the CTA 8-bit log code (~2% steps, floor ≈51 nits).
    /// Ignored for SDR monitors. Added in proto 0.4.
    pub max_nits: u32,
    /// Reserved alignment partner for `max_nits`; must be 0.
    pub reserved0: u32,
}

/// Byte size of `CreateMonitorRequest` before proto 0.4 appended
/// `max_nits`/`reserved0`. Drivers accept this legacy size and treat the
/// missing tail as zeros so 0.3 hosts keep working unchanged.
pub const CREATE_MONITOR_REQUEST_SIZE_V3: usize = 168;

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

/// `UPDATE_MODES` (proto 0.5): REPLACE the set of modes a LIVE monitor
/// currently offers, without a destroy/create cycle.
///
/// **Semantics are REPLACE-TARGET-MODES within the monitor's create-time
/// superset.** `modes[..mode_count]` is the list the monitor should offer
/// from now on. The driver validates every entry against the mode list the
/// session was CREATED with and publishes the survivors as the monitor's
/// IddCx TARGET-mode list (`IddCxMonitorUpdateModes2`).
///
/// **This opcode cannot add a mode the monitor was not created with, and
/// no IddCx DDI can.** `IDARG_IN_UPDATEMODES2` carries target modes only;
/// nothing in IddCx 1.10 or 1.11 replaces an arrived monitor's
/// DESCRIPTION, so its monitor-mode set is frozen at `IddCxMonitorCreate`;
/// and Windows offers the INTERSECTION of the monitor-mode list with the
/// target-mode list (skipped only for remote drivers setting
/// `IDDCX_ADAPTER_FLAGS_REMOTE_ALL_TARGET_MODES_MONITOR_COMPATIBLE`, which
/// a console-session driver cannot be). **Hosts must therefore CREATE the
/// monitor with every mode they might later want** — e.g. both the base
/// rate and the frame-generation-doubled rate — and use this opcode to
/// choose among them. An entry with no counterpart in the create-time list
/// is rejected with detail (see [`UpdateModesReply::rejected`]), never
/// silently accepted.
///
/// Consequences the design depends on:
///
/// - Whatever is published is a subset of a list the EDID already
///   describes, so `modes[0]`'s preferred detailed timing and
///   `PreferredMonitorModeIdx` stay truthful for the life of the monitor.
/// - The published list can never be EMPTY:
///   `IDARG_IN_UPDATEMODES2.TargetModeCount` "cannot be zero"
///   (IddCx.h:3594). `mode_count == 0`, and a request whose entries are
///   ALL outside the superset, are both refused with `err::BAD_MODE` and
///   change nothing.
/// - A refusal is a refused REQUEST. The previously published list stays
///   in force, the monitor is never departed, and the session never fails.
/// - Gating is REVERSIBLE: a later request can publish the wider subset
///   again.
/// - **Gating never evicts the mode the OS has COMMITTED.** A request whose
///   list would leave out the mode the display is currently running is
///   refused ([`err::MODE_COMMITTED`] with [`update_status::BLOCKED`]),
///   because publishing it would force Windows to re-select on a live
///   display — a mode change in the middle of the stream, which is the one
///   thing this opcode exists to avoid. The refusal names the blocking mode
///   ([`UpdateModesReply::blocking_mode_idx`], an index into this monitor's
///   create-time list) and does NOT go away on a retry: publish a subset
///   that keeps that mode, or change the display mode first and gate
///   afterwards.
///
/// The bit depth / dynamic range are monitor-wide and fixed at create —
/// they are derived from the EDID, which cannot be reissued on a live
/// monitor — so this request carries no depth field and the driver
/// validates every entry against the session's existing depth.
///
/// `flags` is a bitmask with no bits defined in 0.5. Drivers IGNORE
/// unknown bits (matching `create_flags` handling), so any future flag
/// that changes semantics must ship with its own `caps::*` bit.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UpdateModesRequest {
    pub session_id: u64,
    /// Reserved bitmask; no bits defined in 0.5. Unknown bits ignored.
    pub flags: u32,
    /// Number of valid entries in `modes` (1..=`MAX_MODES_PER_MONITOR`).
    /// Zero is refused: the target list may be replaced, never emptied.
    pub mode_count: u32,
    /// The target subset to publish, preferred first. Every entry must
    /// match a mode from this session's `CREATE_MONITOR` list.
    pub modes: [ModeSpec; MAX_MODES_PER_MONITOR as usize],
    /// Growth budget; must be 0. Requests may grow by APPENDING after
    /// this (see [`UPDATE_MODES_REQUEST_SIZE_V5`]) because the driver's
    /// reader accepts larger-than-expected input buffers.
    pub reserved: [u32; 4],
}

/// Byte size of [`UpdateModesRequest`] as introduced in proto 0.5.
///
/// Named on day one, unlike [`CREATE_MONITOR_REQUEST_SIZE_V3`] which had
/// to be reconstructed retroactively: when a later minor appends fields,
/// the driver keeps accepting exactly this size and zero-pads the tail.
pub const UPDATE_MODES_REQUEST_SIZE_V5: usize = 80;

/// Flags in [`UpdateModesReply::flags`] (`reserved[2]`).
pub mod update_status {
    /// The merged list is **not in force yet**: the driver queued an
    /// `IddCxMonitorUpdateModes2` push that had not run when the reply was
    /// written (it cannot have — the IRP completes first). Clear means the
    /// modes reported are genuinely what the monitor advertises right now.
    ///
    /// A caller that needs certainty polls `GET_STATUS`: the push's
    /// outcome shows up as the monitor's sticky [`err::UPDATE_FAILED`], and
    /// a failed or deferred push leaves the previous list in force and the
    /// request fully retryable — resending it really does push again.
    pub const PENDING: u32 = 1 << 0;
    /// Fewer modes were accepted than requested, because some requested
    /// entries have no counterpart in the monitor's create-time mode list
    /// and therefore could never be offered. **This is partial success,
    /// not an error**: every mode that exists is published and the session
    /// carries on. Compare [`UpdateModesReply::accepted`] with
    /// [`UpdateModesReply::requested`], and read
    /// [`UpdateModesReply::first_rejected`] for WHICH entry was refused.
    ///
    /// A host seeing this has asked for a mode the monitor was not created
    /// with; the fix is a create-time superset that includes it, not a
    /// retry.
    pub const PARTIAL: u32 = 1 << 1;
    /// **The request was refused permanently — do not retry it.** The
    /// target list asked for would have gated out the mode the OS has
    /// COMMITTED on this monitor, which would force a modeset on a live
    /// display, so the driver refused the push and left the previous list
    /// in force ([`err::MODE_COMMITTED`](super::err::MODE_COMMITTED)).
    ///
    /// This bit exists because a retry loop cannot otherwise tell a
    /// PERMANENT refusal from a transient one: both leave the modes
    /// unchanged and both leave a sticky error behind, but one clears on
    /// the next attempt and this one cannot — it is a property of what the
    /// OS is currently running, not of the attempt. Resending the same
    /// list re-refuses it for as long as that mode stays committed.
    ///
    /// [`UpdateModesReply::blocking_mode_idx`] names the offending mode,
    /// as an index into the list the monitor was CREATED with, so the host
    /// can publish a different subset instead of guessing.
    pub const BLOCKED: u32 = 1 << 2;
}

/// `UPDATE_MODES` reply.
///
/// **`result == OK` means ACCEPTED FOR APPLICATION, not "the OS is now
/// offering these modes."** The driver completes this IRP before it
/// touches IddCx (side effects never run on an IOCTL frame), so the reply
/// structurally cannot carry the `IddCxMonitorUpdateModes2` status. The
/// OS-side outcome arrives later as ETW plus the monitor's sticky
/// [`err::UPDATE_FAILED`] in `GET_STATUS`. What the reply CAN say
/// precisely is whether an application is still outstanding
/// ([`update_status::PENDING`]) and how much of the request the driver
/// took ([`accepted`](Self::accepted) vs [`requested`](Self::requested),
/// with [`rejected`](Self::rejected) and
/// [`first_rejected`](Self::first_rejected) naming what it could not) —
/// so `result == OK` with neither flag set is the one shape that means
/// "exactly what you asked for is offered right now".
///
/// It can also say the opposite precisely: `result ==
/// [`err::MODE_COMMITTED`] with [`update_status::BLOCKED`] set is the one
/// shape that means **stop retrying this list** — the driver refused the
/// push because it would have gated out the mode the OS is running, and it
/// will refuse it again for as long as that stays true.
/// [`blocking_mode_idx`](Self::blocking_mode_idx) names the mode in the
/// way, so the host can pick a different subset. Use
/// [`worth_retrying`](Self::worth_retrying) rather than open-coding the
/// test.
///
/// This struct can never grow: the driver writes replies all-or-nothing
/// and hosts reject a reply whose length differs from what they expect,
/// so a bigger `UpdateModesReply` would break every already-shipped 0.5
/// host with an I/O error rather than degrading. Future fields must come
/// out of `reserved` — which is exactly where the three fields above
/// live, filled in before 0.5 ever shipped and read through the accessors
/// rather than by index.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UpdateModesReply {
    pub session_id: u64,
    /// `err::OK` or a negative `err::*` code.
    pub result: i32,
    /// Modes the monitor will offer once this request is applied: the size
    /// of the published target list. On any error, and whenever
    /// [`update_status::PENDING`] is clear, it is what is offered NOW —
    /// 0 only when the session does not exist.
    pub mode_count: u32,
    /// `[0]` accepted, `[1]` requested, `[2]` flags, `[3]` rejected,
    /// `[4]` first-rejected index (or [`NO_REJECTED_INDEX`]), `[5]`
    /// blocking-mode index (or [`NO_MODE_INDEX`]). Read through the
    /// accessors, never by index.
    pub reserved: [u32; 6],
}

/// [`UpdateModesReply::first_rejected`] when nothing was rejected.
/// Distinguishable from index 0, which is a real rejection of the FIRST
/// requested mode — the case a bare 0 would silently mislabel.
pub const NO_REJECTED_INDEX: u32 = u32::MAX;

/// [`UpdateModesReply::blocking_mode_idx`] when no mode is blocking. Same
/// sentinel as [`NO_REJECTED_INDEX`] and for the same reason: index 0 is a
/// real answer (the monitor's first create-time mode), so "none" needs a
/// value that cannot be one.
pub const NO_MODE_INDEX: u32 = u32::MAX;

impl UpdateModesReply {
    /// A zeroed reply for `session_id`, with the rejection and blocking
    /// indices already at their sentinels. Use this rather than a struct
    /// literal: a literal's `reserved: [0; 6]` would read back as "the
    /// FIRST requested mode was rejected, and your monitor's FIRST
    /// create-time mode is blocking you" — two real and completely
    /// different outcomes.
    pub fn new(session_id: u64) -> Self {
        let mut reserved = [0u32; 6];
        reserved[Self::IDX_FIRST_REJECTED] = NO_REJECTED_INDEX;
        reserved[Self::IDX_BLOCKING] = NO_MODE_INDEX;
        Self { session_id, result: err::OK, mode_count: 0, reserved }
    }

    const IDX_ACCEPTED: usize = 0;
    const IDX_REQUESTED: usize = 1;
    const IDX_FLAGS: usize = 2;
    const IDX_REJECTED: usize = 3;
    const IDX_FIRST_REJECTED: usize = 4;
    const IDX_BLOCKING: usize = 5;

    /// Requested modes the driver published: those with a compatible entry
    /// in the monitor's create-time list. Never >
    /// [`requested`](Self::requested); less means the rest are modes this
    /// monitor was not created with and can never offer.
    pub fn accepted(&self) -> u32 {
        self.reserved[Self::IDX_ACCEPTED]
    }

    /// Requested modes with no counterpart in the monitor's create-time
    /// list. `accepted() + rejected() == requested()` always.
    pub fn rejected(&self) -> u32 {
        self.reserved[Self::IDX_REJECTED]
    }

    /// Index into the request of the first rejected mode, or
    /// [`NO_REJECTED_INDEX`]. Lets a host name the offending mode in a log
    /// instead of only counting it.
    pub fn first_rejected(&self) -> u32 {
        self.reserved[Self::IDX_FIRST_REJECTED]
    }

    /// Echo of the request's `mode_count`, so a caller can detect partial
    /// application by comparison alone, without knowing the driver's cap.
    pub fn requested(&self) -> u32 {
        self.reserved[Self::IDX_REQUESTED]
    }

    /// [`update_status`] bits.
    pub fn flags(&self) -> u32 {
        self.reserved[Self::IDX_FLAGS]
    }

    pub fn is_pending(&self) -> bool {
        self.flags() & update_status::PENDING != 0
    }

    pub fn is_partial(&self) -> bool {
        self.flags() & update_status::PARTIAL != 0
    }

    /// The refusal is PERMANENT while the OS keeps running the mode named
    /// by [`blocking_mode_idx`](Self::blocking_mode_idx): resending this
    /// list cannot succeed. See [`update_status::BLOCKED`].
    pub fn is_blocked(&self) -> bool {
        self.flags() & update_status::BLOCKED != 0
    }

    /// Index into the monitor's `CREATE_MONITOR` mode list of the mode the
    /// OS has COMMITTED and this request would have gated out, or
    /// [`NO_MODE_INDEX`]. Only meaningful with
    /// [`is_blocked`](Self::is_blocked) set.
    ///
    /// An index into the create-time list — not into the request — because
    /// the blocking mode is by definition one the request left out. The
    /// host chose that list, so the index names the mode exactly, in the
    /// one word the reply has left (it may never grow).
    pub fn blocking_mode_idx(&self) -> u32 {
        self.reserved[Self::IDX_BLOCKING]
    }

    /// Is sending this exact request again worth anything?
    ///
    /// THE predicate for a retry loop. `false` only for a
    /// [`BLOCKED`](update_status::BLOCKED) refusal, which is the one
    /// outcome that cannot change until something other than the driver
    /// does — every other failure here (`UPDATE_FAILED`, a deferral, an
    /// I/O error) leaves the previous list in force and re-pushes on the
    /// next attempt.
    pub fn worth_retrying(&self) -> bool {
        !self.is_blocked()
    }

    /// True only when every requested mode is offered by the monitor
    /// **right now** — accepted in full, with no push still outstanding.
    /// The one predicate a host should use before telling a client a mode
    /// is available.
    pub fn fully_in_force(&self) -> bool {
        self.result == err::OK && !self.is_pending() && !self.is_partial()
    }

    /// Set the build-17 detail fields. Kept beside the accessors so the
    /// reserved-word layout has exactly one definition.
    pub fn set_detail(&mut self, accepted: u32, requested: u32, flags: u32) {
        self.reserved[Self::IDX_ACCEPTED] = accepted;
        self.reserved[Self::IDX_REQUESTED] = requested;
        self.reserved[Self::IDX_FLAGS] = flags;
    }

    /// Set the rejection detail (which requested modes the monitor cannot
    /// offer at all). Separate from [`set_detail`](Self::set_detail)
    /// because every outcome fills the counts, while only a rejection
    /// fills these.
    pub fn set_rejected(&mut self, rejected: u32, first_rejected: Option<u32>) {
        self.reserved[Self::IDX_REJECTED] = rejected;
        self.reserved[Self::IDX_FIRST_REJECTED] =
            first_rejected.unwrap_or(NO_REJECTED_INDEX);
    }

    /// Mark this reply as the PERMANENT refusal described by
    /// [`err::MODE_COMMITTED`], naming the committed mode that blocks it by
    /// its index in the monitor's `CREATE_MONITOR` list.
    ///
    /// Sets the flag and the index together — they are one fact, and a
    /// caller that set only the flag would tell a host "stop retrying"
    /// without telling it what to ask for instead, which is the state the
    /// host cannot act on.
    pub fn set_blocked(&mut self, blocking_mode_idx: u32) {
        self.reserved[Self::IDX_FLAGS] |= update_status::BLOCKED;
        self.reserved[Self::IDX_BLOCKING] = blocking_mode_idx;
    }
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
    /// `SlotMetadata::ready_fence_value()` names the shared timeline value
    /// that must be reached before the slot texture is safe to consume.
    pub const READY_FENCE_VALID: u32 = 1 << 2;
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

impl RingHeader {
    pub fn transport_flags(&self) -> u32 {
        self.reserved0
    }
}

impl SlotMetadata {
    /// Build 21 keeps the v1 slot layout stable by storing the additive
    /// 64-bit producer-fence value in the two previously-reserved words.
    pub fn ready_fence_value(&self) -> u64 {
        (self.reserved[0] as u64) | ((self.reserved[1] as u64) << 32)
    }

    pub fn set_ready_fence_value(&mut self, value: u64) {
        self.reserved[0] = value as u32;
        self.reserved[1] = (value >> 32) as u32;
    }
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
    /// Build 23: create flags for the transport actually active in this
    /// generation. Zero means the legacy keyed-mutex transport. Older
    /// drivers left this word zero, preserving backward compatibility.
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
/// updates only touch `x`/`y`/`visible`/`position_qpc`; shape hand-off is
/// a seqlock on `shape_generation`: the writer stores an odd value
/// (rewrite in progress), rewrites the metadata + buffer, then stores the
/// next even value. The reader accepts a copy only when the generation
/// was even and unchanged across it; 0 means no shape published yet.
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

    /// D3D12-openable slot texture. Kept distinct from the legacy keyed
    /// texture so both transports can coexist during host upgrades.
    pub fn slot_texture_d3d12_name(
        session_id: u64,
        generation: u32,
        slot: u32,
        out: &mut [u16; 96],
    ) -> usize {
        out.fill(0);
        let mut i = put(out, 0, "Global\\LuminalVGD-tex12-");
        i = put_hex(out, i, session_id, 16);
        i = put(out, i, "-g");
        i = put_hex(out, i, generation as u64, 8);
        i = put(out, i, "-s");
        put_hex(out, i, slot as u64, 2)
    }

    /// Producer timeline fence shared by every slot in one ring generation.
    pub fn ring_fence_name(
        session_id: u64,
        generation: u32,
        out: &mut [u16; 96],
    ) -> usize {
        out.fill(0);
        let mut i = put(out, 0, "Global\\LuminalVGD-fence-");
        i = put_hex(out, i, session_id, 16);
        i = put(out, i, "-g");
        put_hex(out, i, generation as u64, 8)
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

    // 0.4: 168 (v0.3) + max_nits u32 + reserved0 u32 = 176. The legacy
    // size stays a named constant because drivers accept it forever.
    const_assert_eq!(size_of::<CreateMonitorRequest>(), 176);
    const_assert_eq!(align_of::<CreateMonitorRequest>(), 8);
    const_assert_eq!(CREATE_MONITOR_REQUEST_SIZE_V3, 168);

    const_assert_eq!(size_of::<CreateMonitorReply>(), 160);
    const_assert_eq!(align_of::<CreateMonitorReply>(), 8);

    // 0.5: 8 + 4 + 4 + 4*12 + 4*4 = 80. Locked on introduction so the
    // next growth is a mechanical append + a new SIZE_V<n> constant.
    const_assert_eq!(size_of::<UpdateModesRequest>(), 80);
    const_assert_eq!(align_of::<UpdateModesRequest>(), 8);
    const_assert_eq!(UPDATE_MODES_REQUEST_SIZE_V5, size_of::<UpdateModesRequest>());
    // Replies never grow (see UpdateModesReply docs): if this assertion
    // ever needs changing, the change is a MAJOR-version event.
    const_assert_eq!(size_of::<UpdateModesReply>(), 40);
    const_assert_eq!(align_of::<UpdateModesReply>(), 8);

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
    const_assert_eq!(ioctl::IOCTL_UPDATE_MODES, 0x0022_2024);
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

    /// Build 17 / proto 0.5, both compatibility directions in one place.
    /// The rule is asymmetric (`driver_minor >= host_ANNOUNCED_minor`), so
    /// "it works both ways" has to be asserted, not assumed.
    #[test]
    fn proto_05_keeps_both_directions_working() {
        // OLDER HOST vs BUILD-17 DRIVER. A 0.3 host (build 14 era) and a
        // 0.4 host (build 15/16 era) both still handshake.
        assert!(versions_compatible(PROTO_VERSION_MAJOR, 3, PROTO_VERSION_MAJOR, 5));
        assert!(versions_compatible(PROTO_VERSION_MAJOR, 4, PROTO_VERSION_MAJOR, 5));

        // BUILD-17 HOST vs OLDER DRIVER. This is the direction that would
        // break if the required floor were raised with the minor: a
        // 0.5-announcing host is REFUSED by every 0.3/0.4 driver in the
        // field, and the refusal presents as NOT_HANDSHAKEN on every
        // session IOCTL — a refused session, i.e. a constraint-1
        // violation. Announcing the floor keeps it working.
        assert!(!versions_compatible(PROTO_VERSION_MAJOR, 5, PROTO_VERSION_MAJOR, 4));
        assert!(versions_compatible(
            PROTO_VERSION_MAJOR,
            PROTO_VERSION_MINOR_REQUIRED,
            PROTO_VERSION_MAJOR,
            4
        ));
        assert!(versions_compatible(
            PROTO_VERSION_MAJOR,
            PROTO_VERSION_MINOR_REQUIRED,
            PROTO_VERSION_MAJOR,
            3
        ));
        assert_eq!(PROTO_VERSION_MINOR_REQUIRED, 3, "the floor does NOT move with the minor");
        assert_eq!(PROTO_VERSION_MINOR, 9);
    }

    /// The feature gate a host actually reads. Locked because a wrong bit
    /// here is a capability check that silently lies.
    #[test]
    fn dynamic_modes_cap_is_a_fresh_bit() {
        assert_eq!(caps::DYNAMIC_MODES, 1 << 9);
        // Not the never-set REFRESH_DOUBLING bit, and not colliding with
        // anything already defined.
        for other in [
            caps::HDR10,
            caps::HDR12_BIT,
            caps::SDR10_BIT,
            caps::DIRTY_RECTS,
            caps::REFRESH_DOUBLING,
            caps::HW_CURSOR,
            caps::GAMMA_RAMP,
            caps::MULTI_MODE,
            caps::PERMANENT_POOL,
        ] {
            assert_eq!(caps::DYNAMIC_MODES & other, 0);
        }
    }

    /// The new opcode is APPENDED: the nine shipped IOCTL values must be
    /// byte-identical to what alpha.1 shipped, or every field driver and
    /// host disagree about what a code means.
    #[test]
    fn update_modes_opcode_is_appended_not_renumbered() {
        assert_eq!(ioctl::FN_UPDATE_MODES, 0x809);
        assert_eq!(ioctl::IOCTL_UPDATE_MODES, 0x0022_2024);
        let shipped = [
            ioctl::IOCTL_HANDSHAKE,
            ioctl::IOCTL_CREATE_MONITOR,
            ioctl::IOCTL_DESTROY_MONITOR,
            ioctl::IOCTL_PING,
            ioctl::IOCTL_GET_STATUS,
            ioctl::IOCTL_SET_RENDER_ADAPTER,
            ioctl::IOCTL_QUERY_LEASE,
            ioctl::IOCTL_SET_PERMANENT_POOL,
            ioctl::IOCTL_QUERY_PERMANENT_POOL,
        ];
        assert_eq!(
            shipped,
            [
                0x0022_2000, 0x0022_2004, 0x0022_2008, 0x0022_200C, 0x0022_2010, 0x0022_2014,
                0x0022_2018, 0x0022_201C, 0x0022_2020
            ]
        );
        assert!(!shipped.contains(&ioctl::IOCTL_UPDATE_MODES));
        // And the appended error codes did not renumber an existing one.
        assert_eq!(err::MODE_COMMITTED, -14);
        assert_eq!(err::UPDATE_FAILED, -13);
        assert_eq!(err::BAD_POOL, -12);
        assert_eq!(err::NOT_HANDSHAKEN, -10);
    }

    /// The reply's detail words live in `reserved` because the struct may
    /// never grow, so their positions ARE the ABI. The sentinel matters
    /// most: a zeroed reply must not read as "your first mode was
    /// rejected", which is a real and completely different outcome.
    #[test]
    fn update_modes_reply_detail_words_are_positional_and_have_a_no_rejection_sentinel() {
        let mut r = UpdateModesReply::new(7);
        assert_eq!((r.session_id, r.result, r.mode_count), (7, err::OK, 0));
        assert_eq!(r.rejected(), 0);
        assert_eq!(r.first_rejected(), NO_REJECTED_INDEX, "not index 0");
        assert_eq!(r.blocking_mode_idx(), NO_MODE_INDEX, "not create-time mode 0");
        assert_eq!(NO_REJECTED_INDEX, u32::MAX);
        assert_eq!(NO_MODE_INDEX, u32::MAX);

        r.set_detail(2, 3, update_status::PARTIAL | update_status::PENDING);
        r.set_rejected(1, Some(2));
        assert_eq!(
            r.reserved,
            [2, 3, update_status::PARTIAL | update_status::PENDING, 1, 2, NO_MODE_INDEX]
        );
        assert_eq!((r.accepted(), r.requested(), r.rejected(), r.first_rejected()), (2, 3, 1, 2));
        assert!(r.is_partial() && r.is_pending() && !r.fully_in_force());
        assert_eq!(r.accepted() + r.rejected(), r.requested());
        assert!(!r.is_blocked() && r.worth_retrying(), "partial+pending is retryable");

        // A rejection of the very first requested mode is index 0, and
        // must be distinguishable from "nothing rejected".
        r.set_rejected(1, Some(0));
        assert_eq!(r.first_rejected(), 0);
        r.set_rejected(0, None);
        assert_eq!(r.first_rejected(), NO_REJECTED_INDEX);

        // Clean success is the one shape a host may act on directly.
        let mut ok = UpdateModesReply::new(7);
        ok.set_detail(2, 2, 0);
        assert!(ok.fully_in_force());
    }

    /// The detail fields live INSIDE the reserved words — the reply may
    /// never grow (both sides length-check it), so "report more" has to
    /// mean "report into the space already there".
    #[test]
    fn update_modes_reply_detail_fits_in_the_reserved_words() {
        assert_eq!(core::mem::size_of::<UpdateModesReply>(), 40, "the reply never grows");
        let mut reply = UpdateModesReply {
            session_id: 1,
            result: err::OK,
            mode_count: 4,
            reserved: [0; 6],
        };
        // Plain OK with nothing outstanding is the ONLY shape that means
        // "what you asked for is advertised right now".
        reply.set_detail(2, 2, 0);
        assert!(reply.fully_in_force());
        assert_eq!((reply.accepted(), reply.requested(), reply.flags()), (2, 2, 0));

        // Queued but not applied yet: OK, and NOT in force.
        reply.set_detail(2, 2, update_status::PENDING);
        assert!(reply.is_pending());
        assert!(!reply.fully_in_force());

        // Partial application: real success for what fit, and the counts
        // say so without the caller knowing the cap.
        reply.set_detail(1, 3, update_status::PARTIAL);
        assert!(reply.is_partial());
        assert!(!reply.fully_in_force());
        assert_eq!(reply.result, err::OK, "partial is success-with-detail, not an error");
        assert!(reply.accepted() < reply.requested());

        // The rejection/blocking words are untouched by the counts.
        assert_eq!(reply.reserved[3..], [0, 0, 0]);
        assert_eq!(update_status::PENDING & update_status::PARTIAL, 0);
    }

    /// The PERMANENT refusal has to be readable as such on the wire, and it
    /// has to name the mode in the way. Before it existed, a host that
    /// asked for a subset excluding the committed mode got the same answer
    /// as a transient OS failure — modes unchanged, sticky
    /// `UPDATE_FAILED`, "resend it" — and resending re-refused it forever.
    #[test]
    fn a_blocked_refusal_is_distinguishable_from_a_retryable_failure() {
        // Transient: the OS refused the push, or it was deferred. Nothing
        // is in force yet, but the request is worth sending again.
        let mut transient = UpdateModesReply::new(9);
        transient.result = err::UPDATE_FAILED;
        transient.set_detail(1, 1, update_status::PENDING);
        assert!(transient.worth_retrying());
        assert!(!transient.is_blocked());
        assert_eq!(transient.blocking_mode_idx(), NO_MODE_INDEX);

        // Permanent: the list would evict the mode the OS is running.
        let mut blocked = UpdateModesReply::new(9);
        blocked.result = err::MODE_COMMITTED;
        blocked.mode_count = 2; // the list still in force, untouched
        blocked.set_detail(1, 1, 0);
        blocked.set_blocked(0);
        assert!(!blocked.worth_retrying(), "THE loop-breaker");
        assert!(blocked.is_blocked());
        assert_eq!(blocked.blocking_mode_idx(), 0, "create-time mode 0 is committed");
        assert!(!blocked.fully_in_force());
        // And the two are told apart by BOTH channels, so a host that
        // reads only the code or only the flags still gets it right.
        assert_ne!(blocked.result, transient.result);
        assert_ne!(blocked.flags(), transient.flags());
        // Blocking is orthogonal to the partial/pending words: a refusal
        // still reports how much of the request was even publishable.
        assert_eq!(update_status::BLOCKED & (update_status::PENDING | update_status::PARTIAL), 0);
        assert_eq!(blocked.accepted() + blocked.rejected(), blocked.requested());
        // It rides in the reserved words; the reply still may not grow.
        assert_eq!(core::mem::size_of::<UpdateModesReply>(), 40);
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
    fn d3d12_transport_names_are_generation_scoped() {
        let mut tex = [0u16; 96];
        let mut fence = [0u16; 96];
        names::slot_texture_d3d12_name(0xAB, 7, 2, &mut tex);
        names::ring_fence_name(0xAB, 7, &mut fence);
        assert_eq!(utf16_str(&tex), "Global\\LuminalVGD-tex12-00000000000000ab-g00000007-s02");
        assert_eq!(utf16_str(&fence), "Global\\LuminalVGD-fence-00000000000000ab-g00000007");
    }

    #[test]
    fn ready_fence_value_uses_reserved_slot_words_without_growing_abi() {
        let mut slot: SlotMetadata = unsafe { core::mem::zeroed() };
        slot.set_ready_fence_value(0x1122_3344_AABB_CCDD);
        assert_eq!(slot.ready_fence_value(), 0x1122_3344_AABB_CCDD);
        assert_eq!(core::mem::size_of::<SlotMetadata>(), 80);
    }

    #[test]
    fn ring_header_reports_the_transport_selected_by_build_23() {
        let mut header: RingHeader = unsafe { core::mem::zeroed() };
        assert_eq!(header.transport_flags(), 0);
        header.reserved0 = create_flags::D3D12_FENCE_TRANSPORT;
        assert_eq!(
            header.transport_flags(),
            create_flags::D3D12_FENCE_TRANSPORT
        );
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

// SPDX-License-Identifier: AGPL-3.0-only
//! luminal-vgd-host — what LuminalShine imports to drive LuminalVGD.
//!
//! Three layers:
//! - [`config`] — the control options LuminalShine exposes
//!   (`virtual_display_backend = luminalvgd` plus its sub-options).
//! - [`controller`] — the capture-mode state machine: direct-to-encoder
//!   primary, **seamless** WGC fallback (no Windows toast — see
//!   [`notice`]), and mid-session restore of direct encoding as soon as
//!   the driver is healthy again. Pure logic, fully tested off-Windows.
//! - [`device`] *(Windows only)* — control-device I/O: interface
//!   enumeration, IOCTLs, ring-section mapping.
//!
//! The controller is deliberately synchronous and event-driven: LuminalShine
//! feeds it observations (ring signals, probe results, watchdog trips) and
//! executes the [`controller::Action`]s it returns. No threads, no clocks,
//! no I/O in here — that is what makes the fallback behavior provable.

pub mod config;
pub mod controller;
pub mod notice;
pub mod ring_watch;

#[cfg(windows)]
pub mod device;

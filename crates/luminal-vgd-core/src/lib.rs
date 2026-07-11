// SPDX-License-Identifier: AGPL-3.0-only
//! luminal-vgd-core — platform-independent LuminalVGD driver logic.
//!
//! This crate is the SudoVDA feature port mandated by
//! docs/FEATURE-MATRIX.md: the *behaviors* of SudoVDA's C++ session model,
//! re-specified and implemented in Rust with no code lineage. Everything
//! here is pure logic — no D3D, no IddCx, no registry, no clocks. The
//! driver shell (`luminal-vgd-driver`) injects time, adapters, and storage,
//! which is what makes every behavior in this crate unit-testable on any
//! host OS.
//!
//! Contents:
//! - [`session`] — monitor table: create/destroy/ping, max-monitors cap,
//!   PING-fed watchdog reaping (SudoVDA: default 3 s, 0 = off).
//! - [`modes`] — exact single-entry mode validation (res/refresh/bit-depth/
//!   HDR against handshake caps).
//! - [`adapter`] — render-adapter selection: explicit LUID, else
//!   largest-VRAM default (SudoVDA-compatible).
//! - [`edid`] — per-mode EDID 1.4 generation (SudoVDA shipped a static
//!   high-res EDID blob; we generate one per created monitor instead).
//! - [`ring`] — drop-oldest slot policy with monotonic sequences and
//!   generation bumps (DESIGN.md §3.1/§3.3).

pub mod adapter;
pub mod edid;
pub mod error;
pub mod modes;
pub mod ring;
pub mod session;

pub use error::CoreError;

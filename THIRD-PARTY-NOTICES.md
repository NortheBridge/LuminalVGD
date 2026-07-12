# Third-Party Notices

LuminalVGD is licensed under AGPL-3.0 (with the NALA commercial option —
see LICENSING.md). It incorporates or derives from the following works.
Original code remains available from its authors under its original terms;
the notices below satisfy the attribution requirements of those licenses.

## punktfunk / pf-vdisplay
- Source: https://git.unom.io/unom/punktfunk (crates under
  `packaging/windows/drivers/`, `crates/pf-driver-proto`)
- License: MIT OR Apache-2.0
- Use: architectural and code basis for the LuminalVGD driver core,
  shared-ABI crate pattern, direct-to-encoder transport, and build/signing
  tooling. Copyright © the punktfunk authors. Retain this notice and the
  upstream LICENSE-MIT / LICENSE-APACHE texts alongside any inherited code.

## SudoVDA (SudoMaker)
- Source: https://github.com/SudoMaker/SudoVDA
- License: per upstream README, MIT and CC0/public-domain for SudoMaker's
  changes (Microsoft sample lineage under Microsoft's sample license, MIT).
- Use: **behavioral reference only** — session/monitor lifecycle semantics,
  watchdog, adapter selection, and bit-depth options were re-specified and
  reimplemented in Rust (see docs/FEATURE-MATRIX.md). No SudoVDA source
  code is included. If future contributions translate SudoVDA code closely
  enough to constitute a derivative, verify the upstream LICENSE file at
  that time and expand this notice accordingly.

## Nonary / libvirtualdisplay (Sunshine virtual display stack)
- Source: https://github.com/Nonary/libvirtualdisplay
- License: MIT (Copyright © 2026 Chase Payne)
- Use: behavioral and protocol-design basis for LuminalVGD's proto-v0.3
  feature set — the display-identity/lease split (stable `display_id`,
  connector reservations, identity retention across restarts), per-lease
  configurable timeouts, permanent display pool, physical-dimension EDID
  fields, CTA-861 HDR static-metadata/BT.2020 extension-block structure,
  hardware-cursor capability shape, and the multi-mode-per-monitor model.
  Implemented independently in Rust (`luminal-driver-proto`,
  `luminal-vgd-core`); MIT permits both spec and code inheritance — if
  future contributions port libvirtualdisplay code directly, retain the
  upstream LICENSE text alongside it.

## Microsoft Windows-driver-samples — IndirectDisplay (IddSampleDriver)
- Source: https://github.com/microsoft/Windows-driver-samples
- License: MIT
- Use: reference for IddCx driver structure (indirectly, via the SudoVDA /
  MTT Virtual Display Driver lineage). No sample code included.

## windows-drivers-rs / wdk-sys and related crates
- Source: https://github.com/microsoft/windows-drivers-rs
- License: MIT OR Apache-2.0
- Use: WDK bindings and driver build support for the Rust driver.

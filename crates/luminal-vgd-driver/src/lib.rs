// SPDX-License-Identifier: AGPL-3.0-only
//! luminal-vgd-driver — the LuminalVGD UMDF IddCx driver.
//!
//! # Layering
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────┐
//! │ shell (Windows + eWDK only — phase 2)                        │
//! │  WDF/IddCx callbacks, D3D ring, swapchain worker threads     │
//! ├──────────────────────────────────────────────────────────────┤
//! │ dispatch (this crate, portable, tested)                      │
//! │  IOCTL byte parsing → session logic → reply bytes + effects  │
//! ├──────────────────────────────────────────────────────────────┤
//! │ luminal-vgd-core (portable, tested)                          │
//! │  session table, watchdog, modes, adapters, EDID, ring policy │
//! └──────────────────────────────────────────────────────────────┘
//! ```
//!
//! Everything the driver *decides* lives below the shell line, off-Windows
//! testable. The shell only moves bytes and owns OS handles, per the
//! recovery-first rules of DESIGN.md §3.3:
//!
//! 1. every wait has a timeout (`KMTX_ACQUIRE_TIMEOUT_MS` for slot
//!    acquires; watchdog timer period for everything else);
//! 2. TDR ⇒ rebuild device + ring, bump `ring_generation`, monitors stay;
//! 3. IddCx callbacks never do D3D work inline — they enqueue to the
//!    per-monitor worker and return;
//! 4. `GET_STATUS` is always answerable, even mid-rebuild.
//!
//! # Phase-2 shell map (for the eWDK build; see CLAUDE.md plan)
//!
//! | WDF/IddCx event                         | Calls into                |
//! |-----------------------------------------|---------------------------|
//! | `EvtDriverDeviceAdd`                    | `IddCxDeviceInitConfig` + control queue setup |
//! | `EvtIddCxAdapterInitFinished`           | adapter enumeration → `DeviceState::set_adapters` |
//! | control queue `EvtIoDeviceControl`      | [`dispatch::dispatch`] then applies [`dispatch::Effect`] |
//! | `EvtIddCxParseMonitorDescription`       | serves the EDID from the create effect |
//! | `EvtIddCxMonitorGetDefaultModes` / `QueryTargetModes` | exact single mode from the session table |
//! | `EvtIddCxMonitorAssignSwapChain`        | starts the ring worker (`core::ring::RingPolicy`) |
//! | `EvtIddCxMonitorUnassignSwapChain`      | stops the worker (bounded join) |
//! | watchdog WDF timer (1 s)                | [`dispatch::watchdog_tick`] → unplug reaped monitors |

pub mod dispatch;

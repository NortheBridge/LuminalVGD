# CLAUDE.md — LuminalVGD implementation guide

You are working in the LuminalVGD repository: a Rust UMDF IddCx virtual
display driver for LuminalShine. Read `docs/DESIGN.md`,
`docs/WGC-RELIABILITY.md`, and `docs/FEATURE-MATRIX.md` before writing code.
They are the specification; do not contradict them without flagging it.

## Ground rules
- Language: Rust (windows-drivers-rs / wdk-sys). Do NOT vendor or translate
  SudoVDA C++ source — implement the behaviors specified in
  FEATURE-MATRIX.md. pf-vdisplay Rust code MAY be inherited with notices
  (THIRD-PARTY-NOTICES.md).
- The ABI between driver and host lives ONLY in `crates/luminal-driver-proto`.
  Any layout change bumps `PROTO_VERSION_MAJOR` (breaking) or `_MINOR`
  (additive). Both sides must import this crate; never redefine structs.
- Every wait in driver code has a timeout. No IddCx callback does D3D work
  inline. See DESIGN.md §3.3 — these rules are the project's reason to exist.
- WGC fallback code follows the binding rules in WGC-RELIABILITY.md §Class 2
  verbatim (free-threaded pool, single teardown function, no frame refs
  escaping the handler).
- All commits: SPDX header `AGPL-3.0-only` on new files; the human reviews
  every diff before push.

## Phased plan
1. **Proto crate** — finish `luminal-driver-proto` (handshake, caps, ring
   header, IOCTL codes). Unit-test layout with `static_assertions` on
   size/alignment.
2. **Driver skeleton** — IddCx device that enumerates zero monitors, exposes
   the control device + `HANDSHAKE`/`GET_STATUS`. Installable via
   deploy-dev script on a test VM.
3. **Session model** — `CREATE_MONITOR`/`DESTROY_MONITOR`/`PING`, watchdog,
   max-monitors cap, exact single-mode lists, adapter selection.
4. **Transport** — swapchain acquisition → shared-texture ring with
   keyed-mutex protocol, generation counter, drop-oldest policy.
5. **Host integration** — `luminalvgd` backend in LuminalShine behind
   `virtual_display_backend`; probe → handshake → create → map; encoder
   consumes shared handles.
6. **WGC fallback hardening** — implement the recovery ladder (R1–R6),
   watchdog, reason-coded logging.
7. **Packaging** — INF, catalog, OV signing, FORCE_INTEGRITY clear,
   TrustedPublisher-only installer steps, uninstaller.
8. **Test matrices** — DESIGN.md and WGC-RELIABILITY.md tables become
   scripted/manual test checklists under `tests/`.

## Environment notes
- Driver builds need the WDK + eWDK toolchain on Windows; document exact
  versions in `docs/BUILDING.md` when established.
- Test hardware includes an RTX 5080 host on Insider builds — treat Insider
  regressions as first-class test input, not noise.

## Status & Windows handoff (updated 2026-07-13, main @ b20a492)

Everything portable is built and tested on macOS — 88 unit tests, the
workspace `cargo check`s for `x86_64-pc-windows-msvc`, zero clippy
warnings. Phase 1 is complete; phases 3 and 6 exist as tested logic;
phases 2, 4, 5, and 7 need this Windows box. History: PR #2 = SudoVDA
port + capture controller (seamless, OS-silent WGC fallback with
mid-session restore, DESIGN.md §2.1); PR #3 = libvirtualdisplay fold-in
(proto v0.3: identity/lease split, multi-mode, permanent pool, 256-byte
HDR EDID, cursor ABI, persistence — THIRD-PARTY-NOTICES.md has the MIT
entry).

What exists per crate:
- `luminal-driver-proto` — complete v0.3 ABI, layout-locked. Done.
- `luminal-vgd-core` — every driver decision (sessions, leases,
  identity/connectors, EDID, ring policy, pool, persistence). Done.
- `luminal-vgd-host` — capture controller fully tested; `device.rs` /
  `RingView` compile for Windows but have never executed.
- `luminal-vgd-driver` — `dispatch.rs` (the control plane) is tested;
  **the IddCx shell does not exist yet**.

Ordered plan for this machine (milestones in bold):
1. Build env: eWDK + windows-drivers-rs, UMDF DLL target,
   `bcdedit /set testsigning on`; record versions in docs/BUILDING.md.
2. Phase 2 — IddCx shell in `luminal-vgd-driver`: DriverEntry/WDF device
   add, `IddCxDeviceInitConfig`, control queue → `dispatch::dispatch()`,
   1 s WDF timer → `watchdog_tick()`, DXGI adapters →
   `set_adapters()`, `DeviceState::new(cfg, persisted)` + `startup()`,
   apply `Effect`s. Add a `vgd-probe` CLI (open → handshake → create →
   status → destroy). **Milestone: CREATE_MONITOR shows a monitor in
   Display Settings.**
3. Phase 4 — transport: `EvtIddCxMonitorAssignSwapChain` → worker
   thread (`IddCxSwapChainSetDevice` on the monitor's adapter LUID,
   ReleaseAndAcquireBuffer loop, copy into named keyed-mutex shared
   textures per `proto::names`, publish `SlotMetadata`, heartbeat
   ≤500 ms). `core::ring::RingPolicy` makes all slot decisions; every
   wait bounded; teardown deadline budgeting per DESIGN.md §3.3.
   Shell must register ETW TraceLogging + WPP IFR (§3.3.6).
   **Milestone: ring sequences advance while the desktop animates.**
4. Phase 5 — LuminalShine: `luminalvgd` backend behind
   `virtual_display_backend` using `luminal-vgd-host` (`VgdDevice`,
   `RingView`, `ring_watch::classify`, `CaptureController`).
   **Milestone: Moonlight client streams off the virtual display.**
5. Dev packaging: INF, inf2cat, test-cert signing, deploy-dev script.
   OV signing/TrustedPublisher/FORCE_INTEGRITY stay phase 7; strict
   control-device SDDL (SYSTEM+Admins) is a release blocker (§6).

MVP cuts: SDR 8-bit first; cursor + gamma ramp after first frames; HDR
verified later; WGC fallback needs no new work. Port libvirtualdisplay's
`alttab_stress` for the WGC-RELIABILITY.md §7 race when phase 4 lands.
Merge policy: merge commits, not squash (a squash once orphaned the
luminalshine submodule pointer); luminalshine merges require green CI.

### Phase 2 — COMPLETE (2026-07-16, Windows box)

**Milestone verified: CREATE_MONITOR shows a monitor in Display
Settings**, identity retention reclaims the same connector across
sessions, and the full probe cycle (handshake → create → status → lease →
ping → destroy) passes. Build/sign/install flow: `scripts\
build-driver.cmd` → `scripts\sign-driver.ps1` (eSigner, human-attended) →
`scripts\install-driver.ps1` (elevated) → `cargo run -p vgd-probe
--release`. Caps are SDR-only (`MULTI_MODE | PERMANENT_POOL`) until the
HDR phases.

Hard-won constraints (violating any reproduces a device start failure):
- INF must set `UmdfKernelModeClientPolicy = AllowKernelModeClients`
  (IndirectKmd is a kernel-mode client; without it start fails
  0xC0000182).
- IddCx ≥1.4 clients must wire the *2 DDIs (ParseMonitorDescription2,
  AdapterQueryTargetInfo, CommitModes2, SetDefaultHdrMetaData,
  QueryTargetModes2) even SDR-only.
- `IDDCX_ADAPTER_CAPS.MaxDisplayPipelineRate` must be 0 (u64::MAX fails
  IddCxAdapterInitAsync validation); endpoint friendly name non-NULL.
- No device-object-wide Security SDDL in the INF — the OS graphics stack
  opens IddCx interfaces on the same devobj unelevated. The §6
  control-surface ACL must target the control path only (phase 7).
- ServiceBinary must be `%12%\UMDF\...` — `%13%` run-from-DriverStore
  fails to load on current Insider builds (problem 31).

Diagnostics: ETW provider "NortheBridge.LuminalVGD", GUID
{c501990d-df12-5581-60a8-f55d593d7f7c} (capture: `logman start s -p
"{guid}" -ets -o out.etl`, `pnputil /restart-device`, `logman stop s
-ets`, decode with tracerpt). DriverEntry/DeviceAdd/AdapterInitAsync
breadcrumbs localize any bring-up failure. Deviations to revisit: WPP/IFR
not wired (TraceLogging only); shell state is a process global keyed to
the single root-enumerated devnode.

Next: phase 4 (transport — swapchain worker → shared keyed-mutex texture
ring per plan step 3), then phase 5 (LuminalShine backend).

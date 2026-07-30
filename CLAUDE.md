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

### Phase 4 (transport) — COMPLETE (2026-07-18, Windows box)

**Milestone verified: ring sequences advance while the desktop animates**
(2,108 frames published in a 30 s hold at 4K120, ephemeral identity, no
compositor stalls). The worker GPU-copies each acquired frame into named
keyed-mutex shared textures and publishes through the shared ring
section; `core::ring::RingPolicy` makes every slot decision. Ring state
lives in MonitorRt (sequences/generation survive reassignment); section
is created at plug (SDDL SYSTEM+Admins), textures lazily per frame-desc
(size change ⇒ generation bump).

Bring-up lessons (cost three compositor freezes to learn):
- `IddCxSwapChainReleaseAndAcquireBuffer` returns COM **E_PENDING
  (0x8000000A)**, not STATUS_PENDING, for "no frame yet" — treating it as
  fatal abandons the swapchain mid-activation and stalls the compositor
  until the OS kills WUDFHost.
- On real acquire/publish failure: mark REBUILDING, retire textures,
  **exit the worker** — never retry SetDevice on the same swapchain (the
  OS drives recovery via unassign→assign; holding the dead swapchain
  blocks modeset teardown).
- The OS unassigns+reassigns the swapchain ~10 ms after activation
  (routine); first SetDevice often fails DXGI_ERROR_ACCESS_LOST —
  harmless when the exit path is clean.
- Adapter caps: MaxDisplayPipelineRate=0 AND target-mode
  RequiredBandwidth=0 (nonzero bandwidth vs zero budget makes every mode
  unactivatable: Extend reverts, Scale/Resolution grayed).
- Windows remembers per-identity topology ("Disconnect this display"
  sticks across sessions); vgd-probe --ephemeral mints a fresh identity.

Phase-5 notes: keyed-mutex protocol is key 0 pre-first-publish, key 1
after; readability travels in SlotMetadata.state (mutex only guards
pixels). Reader-side slot-state reconciliation (host CAS
PUBLISHED→READING→FREE, driver honoring shared state) lands with the
consumer. With no reader, drops ≈ published − slots (drop-oldest working
as specified). ETW: FrameLoopStart/RingTexturesCreated/
AcquireBufferFailedExit etc. under the provider GUID above.

Next: phase 5 (LuminalShine `luminalvgd` backend consuming the ring),
then WGC-RELIABILITY §7 alttab_stress port, cursor/gamma/HDR DDIs.

### Phase 5 — lifecycle backend COMPLETE (2026-07-20, Windows box)

**Milestone verified: Moonlight client streams off the virtual
display** — LuminalShine (branch `feat/luminalvgd-backend`) auto-selects
the LuminalVGD backend, creates a per-client monitor (multi-mode:
framegen 240 Hz + base 120 Hz), the display helper applies the
exclusive topology (physical monitors off) at 240 Hz with APPLY acked
in ~1 s, WGC captures the virtual display at the client's native
3456×2160, and both physical monitors restore on session end. Capture
still goes through the WGC helper — the ring-consuming capture backend
is tranche 3b.

Integration lessons (all host-side, none required driver changes):
- LuminalShine's display resolvers/predicates had to learn the NBF
  vendor prefix and "Luminal Video Graphics Display" adapter name; the
  driver-side identity scheme needed nothing.
- Mode-list units: the FFI takes millihertz. LuminalShine normalized to
  mHz and then rescaled ×1000 — Windows silently discards a 240 kHz
  mode, leaving only the base rate. The driver's ParseMonitorDescription2
  / QueryTargetModes2 paths were verified correct via vgd-probe +
  EnumDisplaySettings (both modes register; preferred applies at 240 Hz).
- HDR: the host now requests SDR for VGD displays; asking Windows to
  enable HDR on a monitor without HDR10 caps fails the entire
  SetDisplayConfig apply. Driver HDR10 (EDID metadata + IddCx caps +
  10-bit ring formats) is the gating work for HDR streaming.
- vgd-probe now accepts multiple `WxH@HZ` args (previously the last
  one silently won), so multi-mode creates are testable standalone.

Next: tranche 3b — ring-consuming capture backend in LuminalShine
(`display_vgd` platf::display_t), driver HDR10 caps, cursor/gamma DDIs,
WGC-RELIABILITY §7 alttab_stress port.

### Tranche 3b + HDR10 — COMPLETE (2026-07-20, Windows box)

**Milestones verified the same day:** (1) LuminalShine consumes the
frame ring directly (`display_vgd_vram_t`: claim → keyed-mutex key 1 →
GPU copy → release; no WGC helper, latency parity ~5 ms) and (2) **HDR10
end to end** — driver build 2 (caps 0x185) creates HDR monitors
(bit_depth=110 wire value), Windows engages advanced color off our
CTA-861.3 EDID block, the ring carries FP16 scRGB, and LuminalShine
encodes HEVC Main10 4:4:4 with HDR metadata. AV1 HDR 10-bit also works;
AV1 4:4:4 is an NVENC hardware gap on RTX 5080 (not a software item).

HDR bring-up lessons (one wasted signing round each — check first):
- **IDDCX_ADAPTER_FLAGS_CAN_PROCESS_FP16 is a contract, not a flag.**
  AdapterInitAsync fails STATUS_INVALID_PARAMETER unless the driver also
  registers EvtIddCxMonitorSetGammaRamp (HDR 3x4 matrix; GammaSupport
  must not be NONE) and acquires via IddCxSwapChainReleaseAndAcquire-
  Buffer2 (METADATA2). See "Updates for IddCx 1.10" doc for the full
  obligation list. ETW breadcrumbs localize this in one traced
  pnputil /restart-device — no re-sign needed to diagnose.
- **Proto bit_depth wire values are 8/10/110/112** (HDR carries a
  leading "1"); hdr=1 with bit_depth=10 is BAD_BIT_DEPTH (-4), and the
  host log only shows "result=-4" — check dispatch err codes first.
- Ring textures follow the acquired frame's DXGI format; an
  advanced-color toggle (BGRA8 ⇄ FP16) is a generation bump like a size
  change. Host reader re-latches format automatically.
- Host-side stall detection must key off `latest_sequence` vs the last
  delivered sequence, never cumulative publish counters — an idle
  desktop is indistinguishable from a stall by counters alone.

Next: cursor + gamma DDIs (hardware cursor ABI is in proto v0.3),
WGC-RELIABILITY §7 alttab_stress port, phase 7 packaging (installer,
strict control-device SDDL — release blocker, uninstaller).

### Ring protocol hardening — take-CAS (2026-07-22, build 3)

Live streams stalled ~1 min in (latest_sequence frozen above the last
delivered sequence; LuminalShine's breaker fell back to WGC). Root
cause, found by the threaded protocol regression test in
luminal-vgd-host in milliseconds: the writer picked overwrite victims
from the reconcile snapshot and plain-stored WRITING, silently
clobbering host claims that landed in between — the keyed mutex guards
pixels, but the slot state machine had no atomic hand-off. Fix: the
writer takes slots by CAS (`try_take_slot_writing`, PUBLISHED/FREE →
WRITING) on the same atomic the host claims through; lost takes drop
the frame and reconcile absorbs (Free, READING). Protocol rules the
test enforces forever:

- Every writer path into a slot must win a shared-state CAS first —
  never trust the policy snapshot alone.
- Claims re-read metadata after their CAS (READING protects the slot).
- Consumers deliver only sequence > last-delivered: older published
  leftovers legitimately become "freshest claimable" after the newest
  slot is released, and != dedupe delivers them out of order.
- Host-side health checks key off latest_sequence vs delivered, never
  cumulative counters; a stale heartbeat means "worker stopped"
  (swapchain unassigned during mode switches), not "worker dead" —
  LuminalShine waits a 10 s grace before reinitializing.

Verified: 10-minute vgd-probe --consume soak (0 stalls, autopsy
tooling now built into the probe) + long multi-leg live sessions incl.
Initial-Ping-Timeout reconnect storms.

### Cursor + gamma DDIs, alttab_stress — COMPLETE (2026-07-23, build 6)

**Milestones verified:** (1) hardware-cursor plane end to end — the
driver claims the IddCx cursor (alpha + XOR-emulation, 256²) and
republishes shape/position into the shared cursor section
(`Global\LuminalVGD-cur-<sid>`, seqlock on `shape_generation`);
LuminalShine reads it via `CursorView`/FFI and GPU-blends at encode
time reusing the DDA cursor machinery. Live-validated on the LG OLED
(HDR stream): tracking, hotspot alignment, normal HDR brightness, and
smooth cursor motion over an idle desktop (the driver publishes no
frames for cursor-only changes — `display_vgd` keeps a cursor-free
copy of the last frame and redelivers it with a fresh blend). (2)
`alttab-stress` (WGC-RELIABILITY §7 port): exclusive-fullscreen round
trips on an ephemeral monitor with a concurrent ring consumer + wedge
watchdog — passed with 0 stalls. `caps::GAMMA_RAMP` is advertised;
SetGammaRamp stays acknowledge-and-trace (capture on physical displays
is pre-LUT too, so stream parity is unchanged).

Cursor bring-up lessons (each cost one traced signing round — the
(phase, variant, status) ETW ladder made every round conclusive):
- **IddCxMonitorSetupHardwareCursor fails STATUS_INVALID_PARAMETER
  before a path is committed** (cursor caps are per-path; right after
  MonitorArrival there is no path). Call it at swapchain assign; the
  shell keeps a `cursor_pending` state and retries on every assign.
- **FP16 (HDR) adapters reject QueryHardwareCursor v1 with
  STATUS_NOT_SUPPORTED** — the contract wants the newer variants (v3
  adds the cursor SdrWhiteLevel). The worker discovers the accepted
  variant at runtime (3 → 2 → 1, latched, traced as CursorQueryMode;
  this box latches v3). v2/v3 X/Y are only valid with PositionValid —
  carry the last good pair across visibility-only updates.
- Windows remembers per-identity topology: a stable-identity probe run
  with zero frames + stale heartbeat is usually remembered-disconnect
  state, not a driver bug — re-test with `--ephemeral` before chasing.
- Moonlight/Sunshine has **no client-side cursor channel** — DESIGN.md
  §3.2.3's "forward to the client" is not implementable; the host
  composites server-side at encode time instead (same latency model as
  the DDA path on physical displays).
- **NEVER call into IddCx from inside an IddCx callback, and never
  join a thread that makes IddCx calls without a deadline** (build 7,
  the hard way): IddCx callbacks are win32k callouts, and build 6's
  SetupHardwareCursor retry inside EvtIddCxMonitorAssignSwapChain plus
  an unbounded cursor-worker join in unplug produced mid-stream stream
  drops, persistent RTSP 500s (host TDR gate), unrestorable topology,
  LiveKernelEvent 0x1b8 (win32k callout watchdog) storms every ~72 s,
  and finally an unclean system shutdown. The cursor worker now owns
  every cursor IddCx call (setup retried on its own clock), and
  CursorRt::stop() detaches after 500 ms per §3.3 rule 5. Diagnostic
  signature to remember: 0x1b8 storms in WER = one of our callbacks is
  not returning.

### Phase 7 — packaging & first release (2026-07-23, build 8)

**Control-surface ACL (the §6 release blocker) shipped**: the control
interface is registered with reference string `LuminalVGDControl`;
EvtDeviceFileCreate authorizes control opens under caller impersonation
(`WdfRequestImpersonate` at SecurityIdentification →
`CheckTokenMembership` for SYSTEM / BUILTIN\Administrators — filtered
admin tokens correctly fail), and EvtIddCxDeviceIoControl refuses every
IOCTL on a handle that did not pass (default deny, including handles
opened on the bare device object). OS graphics-stack opens carry other
names and pass unhindered — a device-wide SDDL remains forbidden
(phase-2 lesson). Packaging: install-driver.ps1 gained the §6 OS floor
check (Win11; warn <24H2) and `-SeedTrustedPublisher` (TrustedPublisher
only, never Root); uninstall-driver.ps1 reverses devnode + DriverStore
package (+ optional cert); package-release.ps1 stages the release zip
(gates: valid+timestamped signatures, FORCE_INTEGRITY clear) with
SHA256SUMS; docs/INSTALL.md ships in the zip. Signed artifacts are
release assets only — never committed.

Version identity (one convention, three surfaces): release tag =
SemVer + prerelease (`v0.1.0-alpha.1`); INF DriverVer / Device Manager
= `<semver>.<build>` (`0.1.0.8` — INF versions are four numeric
fields, so the prerelease suffix lives only in the tag), stamped via
LUMINAL_VGD_VERSION + LUMINAL_VGD_BUILD; handshake `driver_build` = the
same `<build>`, bumped every signing round. Unstamped dev builds keep
the date-derived `100.YYMM.DDHH.MMSS` DriverVer.

SudoVDA decision (user, 2026-07-23, FINAL): SudoVDA is unmaintained and
unreliable — no LuminalShine version ships it going forward, and the
LuminalShine installer actively REMOVES SudoVDA whenever detected (new
install, update, or reinstall; drivers/luminalvgd/install.ps1 does the
sweep: devices, DriverStore packages, SudoMaker certs, SudoMaker
registry keys). The MSI bundles the signed LuminalVGD driver-package
as a packaging input instead. LuminalShine's SudoVDA *code* excision
(backend sources, third-party headers, web UI copy) is a tracked
follow-up.

### Control-surface ACL outage — root causes & fix (2026-07-24, build 11)

Build 8's ACL broke streaming on installed deployments: EVERY control
IOCTL (starting with HANDSHAKE) was refused for EVERY caller, so
virtual display creation failed and sessions fell back to letterboxed
physical capture with monitors left on. Fixed in build 11 (validated:
SYSTEM-service handshake `proto 0.3 build 11`, monitor create, ring
capture, live macOS + LG streams). Three independent defects stacked —
each is a permanent rule:

- **IddCxDeviceInitConfig replaces any WDF file-object config
  registered before it.** Our `EvtDeviceFileCreate`/`EvtFileClose` were
  dead code from day one — user-mode opens were only ever gated by the
  kernel device-object DACL, and no handle was ever marked authorized.
  Do not hang ANYTHING off file-create in an IddCx driver; the shell
  now authorizes lazily at IOCTL time (first IOCTL on a handle runs the
  token check against its own caller; fail closed).
- **UMDF impersonation needs BOTH sides to opt in.** Client: CreateFile
  must pass `SECURITY_SQOS_PRESENT | SECURITY_IMPERSONATION`
  (luminal-vgd-host does). Driver: the INF needs
  `UmdfImpersonationLevel = Impersonation` **in the [Install.NT.Wdf]
  section** — in the umdf-service-install section it is SILENTLY
  ignored. Verify after install: `ImpersonationLevel` (=2) must appear
  in the devnode's `Device Parameters\WUDF` registry key, exactly like
  `KernelModeClientPolicy`. Missing either side ⇒ every
  `WdfRequestImpersonate` fails STATUS_ACCESS_DENIED (ETW
  `IoctlDenied stage=1 code=0xC0000022`).
- **Token evaluation is a ladder** (no single point of failure, every
  rung traced): `OpenThreadToken(AsSelf=FALSE)` →
  `OpenThreadToken(AsSelf=TRUE)` → `CheckTokenMembership`; policy is
  TokenUser == SYSTEM or TokenGroups holds BUILTIN\Administrators with
  `SE_GROUP_ENABLED` (filtered admin tokens correctly refused).
  `WdfRequestImpersonate` prefers SecurityImpersonation and retries at
  SecurityIdentification.

Process lessons: a milestone validation counts ONLY against the exact
shipped binary (the phase-7 "elevated probe works" note was wrong for
final build 8 — the ACL had refused every IOCTL since it shipped); and
diagnostics ARE the fix's first half — builds 9-10 existed mainly to
add the ETW auth breadcrumbs and FFI `vgd_last_error()` that made the
real defects visible in one trace each.

Insider caveat (OS 29617, 2026-07-24): user-mode opens of the IddCx
devnode are denied (error 5) below the driver for every non-SYSTEM
caller — INCLUDING elevated Administrators, all four combinations of
{ref-string, bare} × {SQOS, none}, zero driver ETW events, no devnode
security overrides. Elevated vgd-probe / dev-host runs therefore cannot
open the control device on this build; the SYSTEM service path is
unaffected (it's how streaming runs). Re-test on future Insider flights
before chasing "regressions" in our code.

### The ">30-minute install" — script-side scan, not the driver (2026-07-24)

Reported as a suspected driver deadlock after devnode attach; an
adversarial code hunt (11 driver-side hypotheses, all refuted) proved
the driver innocent. setupapi.dev.log showed every device install/start
section completing sub-second — including "Restarting device completed"
in 85 ms — while the 389–392 s gaps BETWEEN sections matched the
install scripts' device-discovery scan exactly: Get-LuminalDevice /
Get-DevicesByHardwareId piped Get-PnpDevice (~500 devnodes on the dev
box) into a PER-DEVICE Get-PnpDeviceProperty round-trip (~0.8 s each ≈
6.5 min per scan). Each script run performs 1–2 scans (install checks
for the devnode then re-verifies; uninstall scans once), so an MSI
update (uninstall + install ≈ 3 scans) plus an attended reinstall
accumulates 2–5 scans at ~6.5 min each — the >30-minute report.
Fix: `-Class Display` pre-narrow (~500 devices → 3, scan → seconds) in
both repo scripts and LuminalShine's drivers/luminalvgd/install.ps1;
also dropped the redundant `/install` from `pnputil /add-driver` (the
UpdateDriverForPlugAndPlayDevices force-bind is the one device
install). Rules:

- Never pipe Get-PnpDevice into a per-device Get-PnpDeviceProperty
  filter without narrowing the device set first. The bulk form
  (`-InstanceId <array>`) silently returns zero rows — it is not an
  alternative.
- Win32_PnPEntity CIM enumeration omits phantom (not-present) devnodes
  (97 on the dev box) — uninstall/sweep paths must stay on
  Get-PnpDevice. Phantoms keep their Class, so class filtering is safe;
  hardware-id matching stays mandatory (ROOT\DISPLAY\0000 here is a
  third-party Root\MetaVirtualScreenDriver).
- Perceived install hangs: read setupapi.dev.log section timestamps
  FIRST — sub-second sections separated by long gaps = tooling between
  the PnP calls, not the driver.
- Latent shell-rule violations found (and refuted as causes of THIS
  symptom) are tracked for the next signing round: unbounded
  Worker::stop join, join-under-monitors-lock in evt_assign,
  apply_effects IddCx calls from callback frames, uncapped cursor setup
  retry, no device-stop teardown, process-global ADAPTER_STARTED.

### Shell callback-hygiene hardening — FAILED COLD-BOOT VALIDATION (2026-07-25, build 12)

Build 12 (this branch, signed and installed 2026-07-25) PASSED every
warm-path check — ETW-verified full sessions: monitor create → assign
storm → frames publishing in <600 ms, clean teardown, zero failure
events (traces in the 2026-07-25 session scratchpad) — but FAILED after
a cold boot: the monitor creates and the ring serves, yet the OS never
activates the display ("device name pending enumeration",
presence=inactive, topology apply leaves ZERO active displays). A/B on
the same booted system: alpha.2 (build 11) streams perfectly; build 12
did not. Confound noted honestly: the alpha.2 install itself rebound
the device on a fully-booted system, and build 12 always worked on warm
rebinds — so the implicated path is COLD-BOOT bring-up, where the
driver initializes concurrently with the OS display stack. Prime
suspect: the deferred adapter bring-up (InitFinished returns
immediately; DXGI walk + set_adapters + ready() now happen on the
effects worker) reordering boot-time initialization. [RESOLVED in
build 13, v0.1.0-alpha.3 "Milestone 1 Update 2", 2026-07-26: root
cause was the ring-lock convoy — confirmed by elimination when alpha.2
passed the cold-boot test — fixed by moving the D3D device create and
IddCxSwapChainSetDevice ahead of the ring lock in frame_loop. Build 13
also reorders the cursor XOR ladder FULL-first (the "black box around
the I-beam" was the OS EMULATION's documented border; EMULATION remains
the fallback) and adds the CursorShape ETW event. Released signed at
user direction; confirm the field checklist on the installed build
(warm stream, COLD BOOT + stream, sleep/resume,
update-over-running-service, I-beam-over-textbox) before LuminalShine
beta.5 ships the pair.] Collateral found the same day (host-side,
tracked in luminalshine): session-start first-frame latency has ~0-4 s
margin against the client's ~10 s no-video deadline (task #32), and the
host heap-crashes (0xc0000374) when capture starts against a
never-activated display with an empty device name (task #33).

### Build 12 cold-boot failure — root-cause analysis (2026-07-26, code-level)

Two independent adversarial reviews of the full build-11→12 diff (plus
live system evidence) narrowed the failure to two surviving hypotheses.
Fast Startup is DISABLED on the dev box (HiberbootEnabled=0, hibernation
off), so the failing boot was a TRUE cold boot with a fresh WUDFHost —
every hiberboot/D3Final-resume theory is dead, and the OS-facing
mode-negotiation/activation code is byte-identical between builds (the
failing binary predates 64a6912's ETW; monitor create+arrival succeeding
proves adapter bring-up completed driver-side).

**H1 (leading, build-12-specific): detach + unbounded ring lock + D3D
create under the lock = activation convoy.** At first activation the OS
routinely does assign → ~10 ms unassign → assign. Worker 1 takes the
ring mutex UNBOUNDED for its whole life (swapchain.rs:313) and calls
create_device_on_luid UNDER it (swapchain.rs:330). Cold (or degraded)
boots make that first D3D create in WUDFHost slow; past 500 ms the
unassign's Worker::stop detaches (swapchain.rs:135-148,
FrameWorkerStopTimeout) leaving the mutex pinned, and assign #2's worker
blocks unboundedly at ring.lock() before it can IddCxSwapChainSetDevice
— the OS modeset transaction never gets its device and rolls the path
back: presence inactive, no GDI name, zero active displays. Build 11
joined unboundedly instead — slow but ORDERED, which is why alpha.2
streamed perfectly the same evening. Corroboration: the host measured
5-10 s NvEnc/D3D device creation that same evening (luminalshine #32
data) — device creation WAS pathologically slow on the failing boot.
Every wait was individually bounded; the COMPOSITION (detach + unbounded
lock + OS call under lock) recreated an unbounded wait on the
activation-critical path. Rule: never hold the ring mutex (or any lock a
teardown path needs) across create_device_on_luid or any OS call; a
detach only ends the wait, not the lock ownership.

**H2 (fallback, pre-existing): boot-time adapter finalization wedge.**
InitFinished returning in microseconds (vs build 11 blocking through the
DXGI walk) lets the OS finalize the indirect adapter at the earliest
boot moment; if the display broker wedges there, it never solicits
parse/query/commit for later monitors. Would exist in build 11's cold
boot too (never tested — the A/B confound).

**VERDICT (2026-07-26): H1 CONFIRMED by elimination — user validated
alpha.2 streams perfectly from a cold boot.** The failure is
build-12-specific; H2 (pre-existing boot-time adapter wedge) is refuted.
Build 13 must carry the convoy fix below; the cold-boot validation gate
stays (H1's trigger — slow first D3D create — is timing-dependent, so
one passing cold boot on build 13 with the fix + ETW confirming
FrameLoopStart after assign #2 is the acceptance bar).

**Original discriminators (for the record):** (a) alpha.2 + cold boot +
stream (free, next reboot): works → H1 confirmed-by-elimination
(build-12-specific); fails → H2-class pre-existing bug. (b) Build-13 cold trace with 64a6912 ETW:
H1 shows AssignSwapChain + FrameWorkerStopTimeout + FrameWorkerSpawned
without FrameLoopStart; H2 shows MonitorArrival then ZERO
ParseDescription2/QueryTargetModes2/CommitModes2. No trace of the
failing evening survives on disk — do not go looking.

**Build-13 fix directions (H1 — correct regardless of verdict):** move
create_device_on_luid BEFORE the ring.lock() in frame_loop (the create
needs no ring state), and/or make the post-spawn ring acquisition a
bounded try-lock poll like mark_ring_dead. Either restores ordered
SetDevice delivery; both together remove the convoy class entirely.
Secondary hardening candidates from the reviews: retry/re-queue for the
one-shot InitFinished skip paths (AdapterReadyStale is currently
permanent until re-add); the one-shot boot-time DXGI adapter list
(set_adapters never refreshes — dispatch.rs:199's comment describes
wiring that does not exist); evt_assign returning STATUS_SUCCESS for
unknown monitors hides drops from the OS. Standing constraint the warm
traces proved: IddCx re-enters the driver synchronously on the effects
thread (SetDefaultHdrMetaData inside IddCxMonitorArrival, UnassignSwapChain
inside IddCxMonitorDeparture) — any DDI handler that blocks on the
effects queue, or an effects task holding a lock a DDI handler takes,
deadlocks.

### Shell callback-hygiene hardening (2026-07-24 — UNVERIFIED, rides next signing round)

Branch `fix/shell-callback-hygiene` (builds clean, NOT yet
signed/installed/live-validated — do not treat as shipped) fixes the six
latent violations above:

- Effects worker thread (`vgd-effects`, control.rs) now owns every
  IddCx/D3D side effect. IOCTL completion, the 1 s watchdog, and
  EvtIddCxAdapterInitFinished only queue (FIFO across producers; traced
  inline fallback if the thread cannot spawn). Adapter bring-up
  (DXGI walk + permanent-pool replug) runs there too; ready() still
  flips only after set_adapters + startup, preserving the NOT_READY gate
  ordering.
- Frame `Worker::stop` joins with the cursor worker's 500 ms detach
  deadline; `evt_assign` stops the old worker OUTSIDE the monitors lock
  and re-checks for unplug-during-assign before storing the new one.
- Cursor SetupHardwareCursor retry gives up after ~5 min
  (`CursorSetupGaveUp`; OS composes the cursor thereafter).
- `EvtDeviceD0Exit(D3Final)` tears down workers/rings (rings marked
  DEAD, no IddCxMonitorDeparture from a power callback) and re-arms
  bring-up; `ADAPTER_STARTED` became a per-WDFDEVICE identity
  (`STARTED_FOR`), so a same-process device re-add re-inits the adapter
  while sleep/resume still skips (IddSampleDriver pattern preserved).

Adversarial review of the branch confirmed and fixed three follow-on
defects the detach semantics would have introduced (rules for any
future detach-style teardown):

- **A deadline-detach leaves the wedged thread's locks HELD.** The frame
  worker pins the FrameRing mutex for its lifetime, so the DEAD-marking
  in unplug/final-exit must never hard-lock() it: `mark_ring_dead()` is
  a 500 ms try_lock poll that skips (traced `RingDeadMarkTimeout`) — the
  host's stale-heartbeat detection covers an unmarked ring. An unbounded
  lock there would have wedged the effects worker forever (silently
  stalling every later effect) or a power callback.
- **Queued tasks can outlive the device.** `AdapterReady` carries an
  adapter-epoch snapshot; `clear_adapter()` (D3Final) bumps the epoch and
  publication is `set_adapter_if_epoch` — a stale task is a traced no-op
  (`AdapterReadyStale`) instead of republishing a destroyed adapter.
  D3Final also clears the WDF device handle so a queued PersistState
  cannot touch a destroyed WDFDEVICE.
- **A detached worker must never touch its swapchain again.** frame_loop
  re-checks the stop flag after the ring-lock wait, after D3D device
  creation (the OS's routine ~10 ms unassign can land inside it), after
  each acquire returns, after publish_frame returns (its texture
  creation and CopyResource are TDR-scale wedge points), and inside
  publish_frame before CopyResource touches the swapchain-owned surface
  (aborting the slot with the same bookkeeping as the keyed-mutex
  timeout arm, released back at the same key) — build 11 made
  post-return swapchain use impossible via the unbounded join; with a
  deadline the checks are what restore that guarantee. Also:
  `STARTED_FOR` rolls back when IddCxAdapterInitAsync fails (failed
  D0Entry gets no D0Exit).
- **Teardown must reconcile portable state, not just runtime.**
  Permanent-pool sessions are lease-disabled — the watchdog can NEVER
  reap them — so D3Final (and the discarded-stale-bring-up path) calls
  `DeviceState::device_teardown_reset()` (unit-tested), or the next
  startup() hits DuplicateSession, creates zero members, and erases the
  desired pool count: pool bricked. Identity reservations survive the
  reset; the desired pool config stays for the next startup.
- **Queued-task validity is captured at issue time.** The AdapterReady
  epoch is captured at D0Entry (stored in `STARTED_FOR`), not inside
  EvtIddCxAdapterInitFinished, and the mark also records the adapter
  object IddCxAdapterInitAsync returned — InitFinished must match it (0
  = store still in flight), so a late callback for a torn-down device
  can neither reuse its own stale epoch nor borrow a replacement
  device's mark. D3Final clears adapter + WDF device FIRST (before the
  multi-second worker drain), so a bring-up racing teardown
  deterministically takes its stale path. The whole epoch/mark protocol
  assumes the single root-enumerated devnode (PnP serializes
  remove-before-re-add); a multi-devnode design needs per-device marks.
- **No stop-target gap during reassign.** evt_assign installs the new
  worker's placeholder (stop flag, no join) in the same lock scope that
  removes the old worker; the join handle is adopted afterwards only if
  session id + monitor identity + the same stop Arc still match
  (`AssignRacedUnplug` otherwise). A teardown landing anywhere in the
  window always finds a stop flag to set. frame_loop also recovers a
  poisoned ring mutex (into_inner) instead of cascading a panic, and a
  PersistState arriving after clear_wdf_device traces
  `PersistSkippedNoDevice` instead of vanishing silently.

Known residual risks, reviewed and accepted for this round (tracked):
single effects worker means one wedged IddCx call stalls all later
effects (trade-off for ordering; ETW `MonitorArrival`-without-
`AdapterReady`-successor patterns would show it); plug still checks the
adapter once at entry (a D3Final landing mid-plug has the same exposure
the pre-hardening code had — a monitor plugged after the D3Final drain
leaks until process end); the effects thread parks in recv() for the
process lifetime (as the frame/cursor workers already did); inline
fallback (worker unspawnable) reverts to callback-frame application
(traced, never-drop-effects trade-off); DESTROY→CREATE reconnect now
serializes A's unplug before B's plug (bounded ≤ ~1.5 s worst case by
the stop deadlines); a PersistState dropped at teardown means the last
pre-removal state change is not persisted (benign — next state change
rewrites the blob); a teardown landing in evt_assign's spawn→adoption
gap stops a join-less placeholder instantly (zero grace for the healthy
new thread — it exits at its next fence, ≤100 ms of calls against a
still-valid-during-unassign swapchain); a plug that raced the final-exit
drain is cleaned up when the next bring-up re-plugs the same session
(displaced-entry stop in monitors::plug), bounded meanwhile by the
cursor give-up cap.

Signing-round validation checklist: streaming end-to-end, permanent-pool
replug at boot, sleep/resume, driver update over a running service,
cursor on the LG, reconnect storm (immediate DESTROY→CREATE), ETW shows
EffectsWorkerSpawned / AdapterReady and no EffectsInlineFallback,
RingDeadMarkTimeout, or AdapterReadyStale.

## Incident 2026-07-27 (read before touching TDR/recovery code)

Real GPU hang during a 4K240 HDR stream → Windows TDR recovery failed →
machine-wide WDDM wedge (QueryDisplayConfig ERROR_NOT_SUPPORTED,
reboot-only; same signature 2026-05-17 under SudoVDA). LuminalShine
beta.5 misclassified 0x887A0004 (DXGI_ERROR_UNSUPPORTED) as TDR in an
unbounded refuse-sessions loop, and its vdd-diagnostic still probes
SudoVDA HWIDs (covered by the tracked SudoVDA code-excision follow-up
above). Driver build 13 exonerated except: handshake advertises
watchdog 3 s but `effective_lease_timeout` floors USE_DEFAULT leases at
10 s (session.rs); CREATE_MONITOR has no surfaced/failed feedback
(zombie sessions when dxgkrnl is dead); and no ETL trace was captured
in the incident window — start a logman session on the provider GUID
above before any repro attempt (WPP/IFR remains the tracked deviation).
Open question: an IddCx virtual display was the exclusive active
display in all three observed wedges (5/17, 7/26, 7/27) — IddCx-class
involvement in failed TDR recovery is not excluded; repro
discriminators are in the postmortem. Full analysis, fix plan (beta.5
blockers + driver build-14 items), and the dev-machine
evidence-collection checklist: `docs/POSTMORTEM-2026-07-27.md`.

### Build 14 — TDR duck-out + watchdog contract (2026-07-27; signed and released as v0.1.0-alpha.4 "Build 14" at user direction)

[Field validation checklist below still to confirm on the installed
build — warm stream, COLD BOOT + stream, sleep/resume,
update-over-running-service, zero Tdr* events on a plain stream —
before the next LuminalShine release ships the pair as default.]

Merged from `feat/tdr-duck-out`. Addresses the incident's IddCx-class open
question from the offensive side: when a frame worker's D3D device
reports removal (GetDeviceRemovedReason — cannot fire on the routine
~10 ms unassign), the effects worker DEPARTS every monitor so a failing
OS TDR recovery can never wait on the indirect display path, parking
identity + ring; a poller probes the RENDER adapter's LUID (never the
default adapter — hybrid-GPU false positive) on throwaway 15 s-deadline
threads, and re-arrives the same monitors (same container GUID +
connector) on recovery. Budgets: one duck in flight (CAS), max 3 cycles
per incident (10-min stability window resets), 10-min recovery budget,
every give-up path drains + dead-marks + clears the pending latch.
Failed departure re-inserts the monitor (never park an arrived
monitor); duck_all self-drains on mid-loop D3Final; plug() purges a
parked twin BEFORE creating its ring section (shared-section name
aliasing). Also DEFAULT_WATCHDOG_SECS 3→10 (advertised == enforced
lease floor; core test locks it). Adversarially reviewed twice: 4
confirmed recovery-path defects fixed + re-verified FIXED; residual
classes documented in the review (all pre-existing accepted).
Validation additions for this round's checklist: ETW
TdrDuckStart(cycle)/TdrDuckDeparted/TdrReplugged/TdrDuckAbandoned(reason)/
TdrRecoveryProbeHung/TdrDuckTornDownMidFlight; a TDR-injection or
driver-verifier-forced device-removal pass would exercise the duck; a
plain stream + reconnect must show ZERO Tdr* events.

### Build 16 — duck the DEVICE, not the DISPLAY (2026-07-30, branch `feat/duck-the-device-build16`; UNSIGNED, UNINSTALLED, UNVALIDATED)

**Build 14's duck-out is not the root cause, but it amplifies.** Verified
2026-07-30 incident: corrected PCIe AER on the GPU root port (09:02:15.111)
→ AcquireBuffer = DEVICE_REMOVED, GetDeviceRemovedReason = DEVICE_RESET
(:18.337) → the Tier-1 duck-out departs the ONLY active display under
`virtual_display_layout=exclusive`, taking the active display count to ZERO
by our own design (:18.421) → dwm.exe reports a BLACK SCREEN 131 ms later
and Windows writes a 0x1b8 live dump (:18.552) → QueryDisplayConfig
SUCCEEDS but returns ZERO paths for 7.9 s, then returns ERROR_NOT_SUPPORTED
and never recovers (:26.569) → dwm.exe is recreated and the replacement
inherits the wedge instantly. Only a power cycle cleared it. **Windows
logged NO Event 4101** — the OS never ran a TDR recovery cycle at all, so
the thing the duck existed to get out of the way of never happened. The
driver never un-ducked (no TdrReplugged/TdrDuckAbandoned for 7 min 3 s):
per spec, TDR_MAX_DUCK_CYCLES=3 with a 10-min budget. Net: a recoverable
device removal became a machine-wide zero-path black screen, and the
departure's DBT_DEVNODES_CHANGED broadcast is the documented GTA V killer.

Build 16 restores DESIGN.md §3.3 rule 2 ("Monitors stay attached"), which
builds 14/15 had deviated from. On device removal the frame worker still
tears down the D3D device, abandons the swapchain, retires textures
(generation bump) and marks the ring REBUILDING — all of which already
happened at swapchain.rs:454-459 — but **the IddCx monitor stays ARRIVED**,
so Windows keeps a display path and no departure is broadcast. Contract
basis (three independent proofs, all verified in-tree): `evt_unassign`
already leaves monitors arrived with no swapchain, and the OS does exactly
that ~10 ms after every activation; `frame_loop`'s failure exit has shipped
since build 3 leaving the monitor arrived while dropping the device; IddCx
binds the device to the SWAPCHAIN via `IddCxSwapChainSetDevice`, never to
monitor arrival.

- **Gate (constraint 1):** `DriverConfig::tdr_duck_mode` (dispatch.rs),
  default `TDR_DUCK_DEVICE`=0. `LuminalVgdTdrDuckMode` REG_DWORD = 1 under
  the devnode's `Device Parameters` key restores the build-14/15 display
  duck-out via `pnputil /restart-device`, NO reinstall. Read at device add
  (`control::read_tdr_duck_mode` — this registry read did not exist before;
  DriverConfig's "read from the registry" doc comment described wiring that
  was never built), mirrored into `Shell::tdr_duck_mode` (AtomicU32) so the
  TDR path never takes the device lock. Absent ⇒ new behaviour;
  out-of-range clamps to default; a read FAILURE is traced with a stage so
  "never configured" and "unreadable" are distinguishable. Deliberately NOT
  seeded by the INF: HKR AddReg rewrites on every package update and would
  silently stomp an operator's override mid-upgrade.
- **Branch point:** `control::queue_tdr_duck(session_id)` — one decision for
  both frame-loop call sites (acquire failure and publish failure), so the
  two modes can never diverge.
- **New path:** `TdrDeviceDuck` adds no global state beyond the existing
  one-in-flight latch and starts a 250 ms-tick poller thread
  (`vgd-tdr-device`). It keeps worker-less rings' heartbeats alive
  (`ring_heartbeat_arc`, single try_lock — a detached worker pins the ring
  mutex), probes the render LUID on throwaway 15 s-deadline threads, and has
  exactly three exits: the OS re-assigned a swapchain on its own (the good
  one — ZERO modesets, display never left) → `TdrDeviceReassigned`; GPU
  healthy but no re-assign within a 10 s grace → ONE `TdrRequalify`
  depart+re-arrive against a HEALTHY stack; 10-min budget expired →
  `TdrDeadlineDepart`, the legacy departure as fallback. Nothing waits
  unbounded and nothing runs on a callback frame.
- **The RING STATE is the recovery discriminator** (`ring_tick_arc`), and
  getting this wrong is the subtle way build 16 fails. Two rejected
  candidates: `worker.is_some()` is useless because `rt.worker` is cleared
  only by unassign/teardown, so the corpse of the very worker that ducked
  reads as "recovered" on the first tick; and counting swapchain
  ASSIGNMENTS is worse than useless, because the OS keeps
  unassigning/reassigning while the GPU is down and each replacement worker
  dies in `create_device_on_luid` — the poller would call that recovered,
  exit, and leave the ring unwatched with the bounded deadline arm
  unreachable. Only `frame_loop` sets ACTIVE, and only after
  `IddCxSwapChainSetDevice` succeeded on a freshly created device, so
  "state != REBUILDING" is the one signal that means the transport really
  came back.
- **…but WHERE that state is read decides whether build 16 works at all,
  and the first cut got it wrong (fixed on this branch, 2026-07-30).** The
  state was read through a `try_lock` on the FrameRing mutex — and
  `frame_loop` PINS that mutex for the entire life of the worker (the
  `let ring = &mut *ring` after the lock is a reborrow, not a drop; it is
  the same invariant that forces `mark_ring_dead_arc` to be a bounded
  try-lock poll). So a ring that had FULLY RECOVERED — new worker, SetDevice
  succeeded, frames publishing — returned `WouldBlock`, which the poller
  counted as pending: the zero-modeset good exit was UNREACHABLE, the
  requalify arm departed the only active display ~10 s into a recovery that
  had already happened, and `TdrDeviceHeartbeatBlocked` fired during healthy
  operation. As written it was WORSE than build 15 — it converted
  self-healing recoveries into forced departures. **Rule: any recovery
  signal a poller reads must be publishable and readable WITHOUT the lock
  the thing being watched owns.** The fix is `crate::tdr::RingLive` (a
  portable, unit-tested `AtomicU32` ring-state mirror + `worker_live` flag +
  `live_generation` counter) living in `swapchain::RingHandle` BESIDE the
  `Mutex<FrameRing>`; `frame_loop` publishes at exactly two points (go-live,
  right where the shared section goes ACTIVE after SetDevice succeeded — not
  earlier, or a worker that then blocks on the ring lock would advertise a
  transport that never starts — and exit, via an RAII `LiveMark` so every
  return path and unwind is covered). `ring_tick_arc` consults the mirror
  FIRST and takes the mutex only to refresh the heartbeat of a ring already
  known to be down, so a lost lock is a lost heartbeat, never a recovery
  verdict. `RingSection::state()` was deleted to keep the old (unreadable)
  reader from coming back. Regression test:
  `tdr::tests::recovered_ring_settles_even_though_its_worker_pins_the_mutex`.
- **Nothing in device-duck mode may depart a display without re-reading the
  mirror immediately first.** The requalify and deadline arms cross the
  effects queue, and the OS can re-assign a swapchain in that gap;
  `run_tdr_requalify` now skips entirely (`TdrDeviceRequalifySkipped`,
  counts as a recovery, consumes no cycle) when nothing is still REBUILDING,
  and both arms depart only the still-unrecovered members of the duck's
  LUID-scoped session set via `monitors::duck_sessions` — build 16 called
  `duck_all` there and threw the per-LUID scoping away at the one moment it
  mattered. The poller's good exit also re-checks AFTER clearing
  `tdr_duck_pending` and re-arms (`TdrDeviceDuckRearmed`) if a worker failed
  inside the clear window, whose `queue_tdr_duck` CAS would otherwise have
  been dropped.
- **`SwapChainDeviceCreateFailed` had no duck wiring at all** and is how a
  GPU death presents when it happens between an unassign and the next
  assign: there is no device yet, so `maybe_queue_tdr_duck` (which
  interrogates one) is unreachable from there. That site now marks the ring
  REBUILDING and arms a duck, gated to device-duck mode. The REBUILDING
  mark is load-bearing, not cosmetic — a duck armed against a still-ACTIVE
  ring settles on its first tick and does nothing. Without this re-arm the
  whole mode degrades into "do nothing, forever": while the GPU is down the
  OS keeps unassigning/reassigning, every replacement worker dies at that
  same site, and nothing would ever be watching the ring again. The re-arm
  is idempotent — `queue_tdr_duck`'s one-in-flight CAS collapses a storm of
  failing workers into a single duck, and the cadence is the OS's reassign
  rate, not a spin. Gated to device-duck mode because the legacy gate must
  restore builds 14/15 faithfully, not an improved version of them.
- **Device-ducked monitors stay in `shell.monitors` and OUT of
  `shell.ducked`.** That invariant is load-bearing: `unplug`'s ducked fast
  path, `plug`'s parked-twin purge and the D3Final drain all assume "in
  `ducked` ⇒ already departed", and a still-arrived entry there would leak
  an arrived monitor on its connector. Keeping it meant none of those three
  needed edits.
- **Honest limitation — there is NO static last-good/black frame, and one is
  not implementable here.** The ring textures were created on the removed
  device and `retire_textures()` already dropped them;
  `create_shared_textures` and `publish_frame` both require a live
  `ID3D11Device`; no CPU-side copy of any frame exists anywhere in the
  driver; and a WARP device's shared handles cannot be opened by the host's
  hardware device. Build 16 ships "display path alive + ring REBUILDING +
  fresh heartbeat" — which the host classifies as *coming back*, not *driver
  gone* (`RING_HEARTBEAT_STALE_MS` = 2000, and before this change NOTHING
  outside `frame_loop` ever called `heartbeat()`). A black-frame publisher is
  a tracked follow-up, not part of this build.
- **New ETW** (existing provider; every legacy Tdr* name kept so legacy-mode
  traces stay comparable): `TdrDuckConfig(mode,source,build)` at device add —
  THE event that makes "which policy is this signed binary running"
  answerable from one trace — plus `TdrDuckConfigReadFailed(stage,code)`,
  `TdrLegacyDuck`, `TdrDeviceDuckStart`, `TdrDeviceDuckStale`,
  `TdrDeviceDuckNoMonitors`, `TdrDeviceReassigned`, `TdrDeviceRecovered`,
  `TdrDeviceHeartbeatBlocked`, `TdrDeviceRequalifyQueued`,
  `TdrDeviceRequalify`, `TdrRequalifyCapped`, `TdrRequalifyStale`,
  `TdrDeviceDuckSessionsGone`, `TdrDevicePollerStale`,
  `TdrDevicePollerSpawnFailed`, `TdrDeviceDuckGaveUp(reason)`,
  `TdrDeadlineDepartStale`, `TdrDeviceTaskQueueFailed(task)` (a poller exit
  arm that could not reach the effects worker — it leaves the monitor
  ARRIVED and deliberately does NOT depart inline, because an IddCx call
  from a poller thread is exactly what §3.3 forbids; silent, it would be
  indistinguishable from an arm never taken), and `RingRebuildMarkTimeout`.
  Note `TdrDeviceReassigned` carries `gpu_confirmed`: whether our own probe
  ever saw the GPU answer, as opposed to the ring merely going ACTIVE
  first — without it, a genuine recovery and a re-assign-into-a-dead-GPU
  read identically in a capture. Added with the lock-free discriminator
  fix: `TdrDeviceRequalifySkipped(covered,publishing)` — the requalify arm
  declining to depart a display that came back, i.e. the acceptance bar
  being met the quiet way — and `TdrDeviceDuckRearmed(session)`; plus
  `TdrDeviceReassigned.live_gens` (go-live transitions, so "a worker really
  came back" is distinguishable from "these rings were never in trouble"),
  `TdrDeviceDuckStart.hresult` / `TdrLegacyDuck.hresult` (the arming
  HRESULT, 0 = poller re-arm), and `kept` counts on `TdrDeviceRequalify` /
  `TdrDeviceDuckGaveUp`.
  Settle these names BEFORE signing — task #58's
  autologger keys on them.
- The dev-fallback `DRIVER_BUILD` was STALE at 14 (build 15 shipped stamped
  by env only), so unstamped dev builds self-reported alpha.4 in ETW and the
  handshake. Bumped to 16 in the same commit.

Acceptance bar for this round: a forced device removal (driver-verifier /
TDR injection) with a live session under `virtual_display_layout=exclusive`
must show `TdrDeviceDuckStart`, ring REBUILDING, and **NO TdrDuckDeparted /
NO MonitorDeparture**; QueryDisplayConfig must keep returning non-zero paths
across the whole window; no dwm.exe black-screen report and no 0x1b8 live
dump. Then the standing checklist: warm stream, COLD BOOT + stream,
sleep/resume, update-over-running-service, `per_client` create/destroy during
an active duck, and ZERO Tdr* events on a plain uneventful stream. Finally
flip the registry gate to 1 and confirm the build-14 behaviour returns
without reinstalling. Validation must run through the SYSTEM service path,
not an elevated probe (Insider-29617 caveat above), and the gate is verified
by reading back the devnode's Device Parameters key. Note build 16 stacks on
ground where the 13/14/15 field checklists are still pending.

### Build 17 — dynamic mode lists (2026-07-30, branch `feat/dynamic-modes-build17`; UNSIGNED, UNINSTALLED, UNVALIDATED)

Branched from `feat/duck-the-device-build16` @ 7a3f696, so build 16 rides
along. **A monitor's advertised mode list can now GROW without a
DESTROY+CREATE cycle** — proto 0.5 `UPDATE_MODES` (`FN 0x809`,
`IOCTL 0x0022_2024`) bound to `IddCxMonitorUpdateModes2`. The motivating
case has no create-time answer: a client streams "Desktop" over Moonlight,
the display exists at the base rate, and only THEN a frame-generation title
launches wanting a doubled rate that was never advertised. The only prior
remedy was a monitor cycle, which broadcasts `DBT_DEVNODES_CHANGED` (kills
GTA V Enhanced via its own uncatchable `0xC000041D` handler) and which
amplified the 2026-07-30 wedge.

Verified before writing any code (do not re-derive): `IddCxMonitorUpdateModes`
= table index 6, `IddCxMonitorUpdateModes2` = 34, `IddFunctionTableNumEntries`
= 36 for 1.10 — in the eWDK header the build compiles against
(`10.0.28000.0/um/iddcx/1.10/IddCxFuncEnum.h:230,258`) AND in our generated
bindings. Both were already emitted by bindgen; only the wrappers were
missing.

- **Additive-merge is the safety property, not a convenience** (`Mode::merge_additive`,
  core/modes.rs). Entries are only APPENDED, never removed or reordered.
  Therefore: `modes[0]` never moves, so the EDID's preferred detailed timing
  — frozen at `IddCxMonitorCreate`, not reissuable on a live monitor — keeps
  describing the mode we still call preferred; and the mode the OS has
  COMMITTED can never disappear, which is what stops an update forcing a
  modeset mid-stream. The driver cannot identify the committed mode
  (`evt_commit_modes2` stores nothing), so "never drop anything" is the only
  available guarantee. Shrinking a live list is deliberately not expressible.
- **Appended modes are `ORIGIN_DRIVER`, not `ORIGIN_MONITORDESCRIPTOR`**
  (`MonitorRt.static_mode_count` splits the list). They demonstrably did not
  come from the frozen EDID, and the OS validates descriptor-origin modes
  against the description it holds — claiming otherwise is a false statement
  it may act on. Create-time modes keep MONITORDESCRIPTOR exactly as shipped.
- **The lock protocol is the whole of the danger.** `IddCxMonitorUpdateModes2`
  makes the OS re-enter `QueryTargetModes2` / `ParseDescription2` /
  `AssignSwapChain` SYNCHRONOUSLY on the calling thread, and all of those take
  `shell.monitors`; `std::sync::Mutex` is not reentrant. `monitors::update_modes`
  takes the lock, publishes the new list (the re-entrant query MUST see it),
  copies the handle, builds the `IDDCX_TARGET_MODE2` array, DROPS the guard,
  then calls. Only the effects worker may call it (`Effect::UpdateModes` →
  `apply_now`) — never an IOCTL or callback frame (§3.3 rule 3; CLAUDE.md:316,548).
  `fill_target_mode2` is now the single fill used by both the push and
  `evt_query_target_modes2`, so pushed and queried lists cannot diverge.
- **Failure degrades to "keep the current modes"** (constraint 1): the previous
  Vec is restored after re-verifying session id AND monitor handle still match
  (the `AssignRacedUnplug` pattern), never a departure, never a refused session.
  The rollback is best-effort by nature — the OS may already have consumed the
  new list in a re-entrant query — so the trace records what happened instead of
  pretending it is atomic. `err::UPDATE_FAILED` (-13) lands in the monitor's
  sticky `GET_STATUS` last error.
- **Three places the list lives, all covered.** `MonitorRt.modes` (live),
  `core::session::Monitor.modes` (durable — what every replug-from-DeviceState
  plugs with; without it a device re-add / D3Final re-bring-up / pool restore
  silently reverts), and `DuckedMonitor.modes` (parked under the legacy TDR
  gate — an update landing while parked patches the parked spec instead of
  calling IddCx, since the re-arrival creates a NEW monitor object).
- **Deferrals rather than refusals**: a duck in flight (`tdr_duck_pending`) or a
  cleared adapter stores the list and skips the OS push — traced — so the
  recovery's own re-negotiation picks it up. Build 16's regression test
  (`tdr::tests::recovered_ring_settles_even_though_its_worker_pins_the_mutex`)
  stays green; nothing was added between the poller and the ring.
- **Versioning, both directions.** PROTO_VERSION_MINOR 4→5;
  `PROTO_VERSION_MINOR_REQUIRED` stays **3** — raising it would make a build-17
  host fail the handshake against every alpha.2/alpha.4 driver in the field,
  presenting as `NOT_HANDSHAKEN` on every session IOCTL, i.e. a refused
  session. Detection is `caps::DYNAMIC_MODES` (1 << 9 — NOT the never-set
  `REFRESH_DOUBLING` bit), which already travels in the handshake, GET_STATUS
  and `VgdCaps.caps`. Reply structs can never grow (all-or-nothing writes on
  one side, exact-length checks on the other), hence `UpdateModesReply.reserved[6]`;
  requests grow by appending, hence `UPDATE_MODES_REQUEST_SIZE_V5` named on day one.
- **`result == OK` means ACCEPTED, not applied.** The IRP completes before the
  effects run, so the reply structurally cannot carry the IddCx status. Do not
  let host code (or docs) claim otherwise.
- ETW (existing provider): `UpdateModesAccepted`, `UpdateModesDenied(stage,code)`,
  `UpdateModesApplied(modes,dynamic,status)`, `UpdateModesDeferred(stage)`,
  `UpdateModesFailed(stage,code,rolled_back)`, plus a `dynamic` field added to
  `ParseDescription2` / `QueryTargetModes2`. Note this is the FIRST ETW at the
  dispatch layer at all — nothing there traced anything before, which is how
  the build-8 ACL outage stayed unexplained for three builds.
- Found and fixed while testing: the UPDATE_MODES arm validated the OUTPUT
  buffer only at reply-write time, so a short output buffer mutated the session
  table while the effect was dropped with `BadBuffer` — permanently diverging
  the durable list from the advertised one. The arm now checks the output size
  before touching the table. (CREATE_MONITOR has the same shape; harmless there
  — the session just exists un-plugged and the watchdog reaps it — and left
  alone deliberately.)

**THE OPEN QUESTION, and it can invalidate the feature.** `IDARG_IN_UPDATEMODES2`
carries TARGET modes only. `IddCx.h:258-264` says the OS skips
`ParseMonitorDescription2` ONLY for remote drivers setting
`REMOTE_ALL_TARGET_MODES_MONITOR_COMPATIBLE` — which a console-session driver
(entry.rs sets only CAN_PROCESS_FP16) is not and cannot be. So the presented
list stays monitor∩target, and an ADDED mode surfaces only if the OS
re-solicits the parse DDI after the update. Our parse/query handlers read the
live list, so a re-parse is sufficient — but NOTHING in the 1.10 headers says
whether one happens, and no entry point in the whole 36-entry table updates a
monitor description. Equally undocumented: whether an update broadcasts a
devnode change (the header says only "An OS callback function the driver calls
to update the mode list"). **Do not assert either way in code or docs until
measured.** One traced install answers both: `vgd-probe 2560x1440@120 --hold 30
--add-mode 2560x1440@240 --add-after 10` with a logman session on the provider
GUID — look for `UpdateModesApplied`, then whether `ParseDescription2` /
`QueryTargetModes2` reappear with `dynamic=1`, whether the added rate shows up
in Display Settings, and whether any devnode-change follows. If the OS does not
re-parse, dynamic ADD is not achievable through this DDI for a console-session
driver and the approach has to change before more is built on it. Everything
shipped here is still correct and safe in that case — it just would not surface
a new mode.

Host-side work remaining (LuminalShine, NOT done here): the pinned submodule
`src/drivers/luminal-display` is at da0349b = build 15 / proto 0.4, so it cannot
even see build 16 — any host work needs that pointer advanced first. Then the
call site at `virtual_display_vgd.cpp:361` (which today can only advertise the
base rate at CREATE time, and only if framegen is already known active) gains an
`UPDATE_MODES` path gated on `VGD_CAP_DYNAMIC_MODES`, degrading silently in the
style of the existing `proto_minor < 4` nits log.

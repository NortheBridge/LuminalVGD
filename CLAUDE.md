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

  > **⚠ REFUTED ON BUILD 29617 (corrected 2026-07-30 by debugging 25 dumps
  > with cdb). Do not reason from the rule above on current Windows.**
  > On this build `0x1b8` is `VIDEO_MINIPORT_BLACK_SCREEN_LIVEDUMP`, not a
  > win32k callout watchdog. All 25 dumps across 2026-07-29/30 carry Arg1=0xa
  > and a byte-identical stack:
  > `dwm.exe → NtGdiDdDDIEscape → dxgkrnl!DxgkEscape →
  > win32kbase!xxxDisplayDiagBlackScreenDetected → dxgkrnl!DxgkCheckDisplayState
  > → dxgkrnl!DxgCreateLiveDumpWithDriverBlob → watchdog!WdDbgReportCreate`.
  > `watchdog.sys` appears **only as the report writer**, called *by* dxgkrnl
  > with `0x1b8` passed as an ordinary argument — there is no watchdog wait
  > anywhere, and the single captured thread is dwm.exe **running**, not
  > blocked. dwm is voluntarily reporting a black screen.
  >
  > So on 29617 a `0x1b8` storm counts **black-screen symptoms, not hung
  > callbacks**. The rule was true when `0x1b8` genuinely was a callout
  > watchdog on an older OS; it is a false lead now, and it sent one
  > investigation looking for a hung IddCx callback that did not exist.
  >
  > Two further corrections from the same session:
  > - The `4400/4401/4402/4403` in `WATCHDOG*.dmp` filenames are **rotating
  >   WER slots, not bugcheck subcodes** (caught live: `4403` at 20:23:02.260
  >   followed by `4400` at 20:23:02.801 — 541 ms apart, same process, same
  >   stack).
  > - These are 256 KB kernel **triage** dumps: one process record, one
  >   thread, one call stack. `!process 0 0` fails and `!stacks 2` returns
  >   nothing. They can neither implicate nor exonerate this driver, and any
  >   claim in either direction from them is unsupported. Driver-side ground
  >   truth comes from our own ETW provider, not from these.

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

> **REWRITTEN 2026-07-30 (second model).** Everything below the
> "REPLACE-TARGET-MODES" heading supersedes the additive-merge design that
> the rest of this section originally described; the historical text is
> kept only where it still documents a live rule. Read the new part first.
>
> **AMENDED 2026-07-31 (review pass, `f04cb2a`/`c0daafb`).** Five findings
> fixed; the two that changed the DESIGN are called out under
> "Review-pass corrections" at the end of this section. If you are about to
> reason about the parked-spec patch, about what the OS has committed, or
> about `PreferredMonitorModeIdx`, read that first — three of the bullets
> below are now historical on those points.

Branched from `feat/duck-the-device-build16` @ 7a3f696, so build 16 rides
along. **Which of a monitor's create-time modes it offers can now change
without a DESTROY+CREATE cycle** — proto 0.5 `UPDATE_MODES` (`FN 0x809`,
`IOCTL 0x0022_2024`) bound to `IddCxMonitorUpdateModes2`. The motivating
case: a client streams "Desktop" over Moonlight and only THEN a
frame-generation title launches wanting the doubled rate on offer. The
only prior remedy was a monitor cycle, which broadcasts
`DBT_DEVNODES_CHANGED` (kills GTA V Enhanced via its own uncatchable
`0xC000041D` handler) and which amplified the 2026-07-30 wedge.

#### REPLACE-TARGET-MODES — the corrected model (do not re-derive)

Established from the 1.10 headers, the IddCx 1.11 update page, the
`IddCxMonitorUpdateModes2` reference page, and the
VirtualDrivers/Virtual-Display-Driver source (IddCx 1.10, MIT, calls
UpdateModes ZERO times):

- `IDARG_IN_UPDATEMODES2` (IddCx.h:3586) carries ONLY
  `{ Reason, TargetModeCount, pTargetModes }`. **TARGET** modes.
- There is **NO DDI in 1.10 or 1.11 that replaces a monitor's DESCRIPTION**
  on an arrived monitor. The monitor-mode set is fixed at
  `IddCxMonitorCreate`. Changing it means departure + recreate.
- `IDDCX_ADAPTER_FLAGS_REMOTE_ALL_TARGET_MODES_MONITOR_COMPATIBLE` (which
  would skip the intersection) is documented remote-drivers-only; a console
  driver cannot opt in (`REMOTE_SESSION_DRIVER` fails adapter init).
- Windows selects from the **INTERSECTION** of the monitor-mode list and
  the target-mode list.

Therefore: advertise a static SUPERSET of monitor modes at creation
(EDID → `ParseMonitorDescription2`), then use `IddCxMonitorUpdateModes2`
to publish the currently valid TARGET subset. **It can gate/steer within
the superset; it can never enlarge it.** Hosts must CREATE with every mode
they might later want (base rate *and* framegen rate). A requested target
with no entry in the superset is rejected with detail (`rejected`,
`first_rejected`), never published. **`TargetModeCount` cannot be zero**
(IddCx.h:3594) — the target list is replaceable, never emptyable; both
`mode_count == 0` and an all-out-of-superset request are `err::BAD_MODE`
with the published list untouched. `Reason` is
`IDDCX_UPDATE_REASON_CONFIGURATION_CONSTRAINTS` (IddCx.h:327).

**Function-table index 6 (`IddCxMonitorUpdateModes`) is DELETED from
bindings.rs and must never be re-added.** Verbatim from the
`IddCxMonitorUpdateModes2` reference page: "drivers reporting
IDDCX_ADAPTER_FLAGS_CAN_PROCESS_FP16 can only call
IddCxMonitorUpdateModes2; calling IddCxMonitorUpdateModes is an error."
CAN_PROCESS_FP16 is the ONLY adapter flag `entry.rs` sets
(`caps.Flags = ...CAN_PROCESS_FP16`, entry.rs:251 — the HDR10 contract),
so index 6 is forbidden for this driver as long as it does HDR at all. The
comment block where the wrapper used to be carries the citation.

State split, three layers, each with two lists now:
`Monitor.modes` (durable superset, frozen) + `Monitor.target_modes`
(durable published subset, seeded to the whole superset at create, carried
by `Effect::PlugMonitor.targets` so a replug never silently ungates);
`MonitorRt.monitor_modes` + `MonitorRt.target_modes`;
`DuckedMonitor.monitor_modes` + `.target_modes`. `static_mode_count` and
`ORIGIN_DRIVER` are GONE — every monitor mode comes from the EDID that
created the monitor, so `MONITORDESCRIPTOR` is now simply true.
`ParseDescription2` serves the superset, `QueryTargetModes2` the subset.

**REPLACE vs APPEND is still undocumented, and it no longer matters for
safety.** The Learn Remarks say only "update the mode list previously
reported for a monitor"; the headers say nothing. The code ASSUMES REPLACE
(recorded at `shell::monitors::push_targets`) and is correct either way
because of one invariant re-checked immediately before the OS call against
the LIVE `monitor_modes`: `targets ⊆ superset`. Replace ⇒ OS holds
`targets`; append ⇒ `previous ∪ targets`; re-solicit ⇒ `targets`. All
three are non-empty subsets of the frozen superset, so the intersection is
non-empty and fully activatable — no unactivatable mode, no monitor
without targets, no failed session. Only effectiveness differs (append
would fail to remove a rate). **How a reader tells:** publish a strict
subset, then in one trace read `UpdateModesApplied(modes, superset)`,
whether `QueryTargetModes2` reappears with the pushed count, and how many
rates Display Settings offers — `published` ⇒ replace/re-query,
`superset` ⇒ append or no re-solicit. `vgd-probe --target-mode WxH@HZ`
(alias `--add-mode`, same flag, new meaning) exercises it standalone.

ETW changed with the model: `UpdateModesApplied(modes, superset, status)`;
`ParseDescription2(modes, published, buffer)` and
`QueryTargetModes2(modes, superset, buffer)` replace the old `dynamic`
field; `UpdateModesAccepted` / `UpdateModesDenied` gained `rejected` (and
Denied gained `first_rejected`). New deny stage
`UPD_STAGE_NOT_IN_SUPERSET = 9`. Settle these names BEFORE signing.

#### Historical (additive-merge, superseded — kept for the rules that survived)

Verified before writing any code (do not re-derive): `IddCxMonitorUpdateModes`
= table index 6, `IddCxMonitorUpdateModes2` = 34, `IddFunctionTableNumEntries`
= 36 for 1.10 — in the eWDK header the build compiles against
(`10.0.28000.0/um/iddcx/1.10/IddCxFuncEnum.h:230,258`) AND in our generated
bindings. Both were already emitted by bindgen; only the wrappers were
missing. (Index 6's wrapper has since been DELETED — see the
CAN_PROCESS_FP16 rule above. Index 34 is the only legal entry.)

- ~~**Additive-merge is the safety property**~~ — SUPERSEDED. `merge_additive`
  is gone; `Mode::select_targets` replaced it. The reasoning was applied to
  the wrong list: appending to what the code called "the monitor's modes"
  could never enlarge the frozen description, so it bought nothing the OS
  would honour. ~~The *concern* it addressed survives and is now explicit —
  the driver still cannot identify the committed mode
  (`evt_commit_modes2` stores nothing), so gating a rate the OS has
  committed will make it re-select. That is now a deliberate,
  host-requested effect rather than something the design forbids.~~
  **SUPERSEDED 2026-07-31** — the driver DOES identify the committed mode
  now, and refuses the push instead of re-selecting. See "Review-pass
  corrections" below.
- ~~**Appended modes are `ORIGIN_DRIVER`**~~ — SUPERSEDED, along with
  `MonitorRt.static_mode_count`. Nothing is ever appended to the monitor
  list, so every monitor mode really does come from the EDID that created
  it and `MONITORDESCRIPTOR` is unconditionally truthful
  (`monitors::MONITOR_MODE_ORIGIN`).
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
- **Three places the list lives, all covered** — and since the rewrite each
  holds BOTH lists: `MonitorRt.{monitor_modes,target_modes}` (live),
  `core::session::Monitor.{modes,target_modes}` (durable — what every
  replug-from-DeviceState plugs with; without the target half a device
  re-add / D3Final re-bring-up / pool restore silently un-gates), and
  `DuckedMonitor.{monitor_modes,target_modes}` (parked under the legacy TDR
  gate — an update landing while parked patches the parked target subset
  instead of calling IddCx, since the re-arrival creates a NEW monitor
  object, and re-checks it against the parked superset first).
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
- **NOTHING COMMITS UNTIL THE OS TAKES IT (fixed 2026-07-30, second pass).**
  The first cut committed the DURABLE list inside `dispatch` — before the push,
  and unconditionally. Three symptoms, one defect: a failed push rolled back
  only the RUNTIME list, so durable and runtime diverged; a DEFERRED push
  (duck in flight / adapter cleared) left durable asserting modes the OS was
  never told about; and worst, the identical RETRY then merged to "nothing to
  add", emitted no effect, and returned `OK` — a permanent silent no-op
  reporting success while the monitor advertised the old list, with no way for
  the caller to recover. The contract now (unchanged by the rewrite except
  for which list it guards): `SessionTable::update_modes` parks the selection
  in `Monitor.pending_targets` with a table-wide monotonic `update_seq`;
  `Effect::UpdateModes` carries that seq; `monitors::update_modes`
  is the only caller of `IddCxMonitorUpdateModes2` and owes exactly ONE
  `settle_modes`, with `Applied` (and only `Applied`) committing
  `Monitor.target_modes`. Failed AND deferred both settle `NotApplied` →
  pending discarded, every copy keeps the pre-update list, sticky
  `err::UPDATE_FAILED`, and the next identical request genuinely re-pushes.
  Rules that fall out and must not be re-broken: a deferral is NOT an
  application (~~the parked-spec patch is best-effort and still settles
  NotApplied — a retry re-selects to the same list, so they converge~~ —
  **SUPERSEDED 2026-07-31**: the parked-spec patch COMMITS, because it
  changes what the re-arrived monitor publishes; a deferral is now
  strictly "nothing was changed anywhere". See "Review-pass corrections"); a
  request arriving while a push is outstanding replaces the PENDING
  selection, never the live one (replace semantics: last intent wins, and
  the effects worker is serialized so push #1 finishes before push #2
  starts); a stale settle (superseded, or session destroyed and the id
  reused) commits nothing, which is why the seq is table-wide and never
  reset. Residual, accepted and documented at `settle_modes`: a superseded
  push that SUCCEEDED followed by a superseding one that FAILED leaves the
  durable subset lagging the OS's until the next request — both are valid
  non-empty subsets of the superset, so nothing unactivatable results.
- **Partial application is reported, not swallowed.** `Mode::select_targets`
  returns `TargetSelection { targets, accepted, rejected, first_rejected }`
  with `accepted + rejected == requested.len()` always. `UpdateModesReply`
  fills its `reserved` words — `[0] accepted, [1] requested, [2] flags,
  [3] rejected, [4] first_rejected` — read through
  `accepted()/requested()/flags()/rejected()/first_rejected()/is_pending()/
  is_partial()/fully_in_force()`, never by index; build with
  `UpdateModesReply::new`, because a literal `[0; 6]` reads back as "your
  FIRST mode was rejected" (`NO_REJECTED_INDEX` = `u32::MAX` is the sentinel).
  The struct does NOT grow (still 40 bytes, asserted). `update_status::PARTIAL`
  now = some requested modes are not in the create-time description and can
  never be offered; `PENDING` = queued at the OS, not in force yet. Partial
  stays `err::OK`: never fail the session, never drop the modes that DO
  exist (constraint 1). `result == OK` with neither flag is the ONLY shape
  meaning "in force, in full, right now". Total rejection is `err::BAD_MODE`
  with the same detail — a refused REQUEST, not a failed session.
- **The rollback can only undo its own write**: the failure path restores
  only when the monitor handle still matches AND the runtime target list is
  still, entry for entry, the one this call published. What it puts back was
  itself a non-empty subset of the same frozen superset, so a rollback can
  produce neither an empty nor an unactivatable target list.
- ETW (existing provider): `UpdateModesAccepted(modes,queued,accepted,
  requested,rejected,pending,partial)`,
  `UpdateModesDenied(stage,code,modes,rejected,first_rejected)`,
  `UpdateModesApplied(modes,superset,status)`,
  `UpdateModesDeferred(stage,modes,retryable)`,
  `UpdateModesFailed(stage,code,modes,rolled_back)`,
  `UpdateModesSettleStale(seq)` (a push whose update was superseded or whose
  session is gone — silent, it would be indistinguishable from a settle that
  never happened, and a missing settle is the one way to leak a pending list),
  plus `published` on `ParseDescription2` and `superset` on
  `QueryTargetModes2` (the pair that answers replace-vs-append).
  Note this is the FIRST ETW at the dispatch layer at all — nothing there
  traced anything before, which is how the build-8 ACL outage stayed
  unexplained for three builds. Settle these names BEFORE signing.
- Found and fixed while testing: the UPDATE_MODES arm validated the OUTPUT
  buffer only at reply-write time, so a short output buffer mutated the session
  table while the effect was dropped with `BadBuffer` — permanently diverging
  the durable list from the advertised one. The arm now checks the output size
  before touching the table. (CREATE_MONITOR has the same shape; harmless there
  — the session just exists un-plugged and the watchdog reaps it — and left
  alone deliberately.)

**THE OPEN QUESTION AS FIRST WRITTEN — now ANSWERED, and it is what forced
the rewrite above.** The original text asked whether the OS re-solicits
`ParseMonitorDescription2` after an `IddCxMonitorUpdateModes2`, hoping a
re-parse would let an ADDED mode survive the monitor∩target intersection.
That hope was unfounded: `ParseMonitorDescription2` is handed the EDID the
monitor was CREATED with, and no DDI reissues it, so re-parsing could only
ever return the same superset. Dynamic ADD is not achievable through this
DDI for a console-session driver, full stop. The feature is now
replace-target-modes within the create-time superset (above), which is
achievable and is what the traced install will measure.

The measurement command changes with it:
`vgd-probe 2560x1440@120 2560x1440@240 --hold 30 --target-mode 2560x1440@240
--target-after 10` with a logman session on the provider GUID — create with
BOTH rates, then publish only the doubled one. Look for
`UpdateModesApplied(modes=1, superset=2)`, then whether
`QueryTargetModes2` reappears, whether Display Settings drops to one rate
(⇒ REPLACE) or keeps both (⇒ APPEND / no re-solicit), and whether any
devnode-change follows. Still undocumented and still not to be asserted
either way in code or docs until measured: the devnode-change question, and
replace-vs-append (safe either way — see the invariant above).

#### Review-pass corrections (2026-07-31, build 17 still UNSIGNED)

Five review findings, all verified against the code before being fixed.
The decisions moved into `crate::modepush` — portable, unit-tested,
below the shell line for exactly the reason `crate::tdr` is:
`shell::monitors::push_targets` needs the eWDK, a live adapter and an
arrived monitor, so it can never be tested, and every decision it made
inline was therefore untested. Both MAJOR fixes have a test that fails
against the pre-fix behaviour
(`modepush::tests::a_parked_patch_commits_durably_so_a_rescind_still_re_pushes`,
`modepush::tests::a_push_that_would_evict_the_committed_mode_is_refused`).

- **A TDR-parked patch is an APPLICATION, not a deferral** (supersedes
  "the parked-spec patch is best-effort and still settles NotApplied"
  above). `push_targets` patched `DuckedMonitor.target_modes` — so the
  monitor the replug re-arrived published the new subset — and returned
  `Deferred`, so `Monitor.target_modes` kept the old one. Because
  `SessionTable::update_modes` short-circuits a request matching the
  durable list ("already published", no effect, `OK`), the RESCIND
  direction was unreachable for the life of the session: the host asked
  for the wider list back, was told yes, and kept streaming to a gated
  monitor. `PushOutcome::commits()` is now both the authorisation to write
  the parked spec and the settle decision, so the halves cannot drift
  apart; `AppliedParked` is a separate variant from `Applied` only so a
  call-less application does not pollute `UpdateModesApplied(modes,
  superset)`, which is the replace-vs-append measurement. This also closed
  the MINOR "patched runtime, never committed durably" — same defect,
  other side.
- **The committed path is captured, and a push never gates it out**
  (supersedes "the driver still cannot identify the committed mode … that
  is now a deliberate, host-requested effect"). `evt_commit_modes2`
  discarded `IDDCX_PATH2`; it now records the ACTIVE paths into
  `modepush::CommittedPaths` (lock-free fixed slots, atomic stores, no
  allocation — it is a modeset CALLBACK FRAME) and traces every path with
  its mode (`CommitModes2Path`). **`UPDATE_MODES` steers what the OS MAY
  select; it never evicts what the OS HAS selected**: under
  `virtual_display_layout=exclusive` that would force a modeset on the
  only active display, mid-stream, which is the one thing this feature
  must not cause. Such a push is refused (`GATES_COMMITTED`) — the PUSH
  only, never the session. A host that wants a different ACTIVE mode does
  the modeset itself (`SetDisplayConfig`, which the display helper already
  drives) and gates afterwards. FAILS OPEN by design: a committed mode the
  superset does not describe cannot be reasoned about, so the push
  proceeds as before and the trace says so — otherwise one decoding
  mismatch would silently disable the whole feature.
- **D3Final can no longer tear the adapter down under a live push.** It
  runs on a power callback concurrently with the effects worker; checking
  `adapter()` before the call left the whole interval to the DDI
  unguarded. Handshake: the push marks in-flight → fence → reads handle
  and epoch in ONE acquisition (`Shell::adapter_with_epoch`); `evt_d0_exit`
  clears the adapter → fence → drains in-flight pushes on a 500 ms
  deadline (`UpdateModesDrainTimeout`) inside the worker drain it already
  performs. SeqCst fences both sides ⇒ at least one sees the other. The
  epoch is re-checked after publishing and before the call
  (`ADAPTER_TORN_DOWN`: restore the runtime list, defer).
- **`PreferredMonitorModeIdx` follows the published subset.** It was a
  constant 0 while a gate could exclude `monitor_modes[0]`, naming a mode
  outside the intersection Windows offers. Now the first monitor mode
  actually offered, computed against what was written into the OS's
  buffer, and traced as `preferred` on `ParseDescription2`.
- New ETW (settle before signing, task #58's autologger keys on these):
  `CommitModes2Path(monitor,flags,active,width,height,refresh_mhz)`,
  `CommitModes2` + `active`/`skipped`/`generation`, `UpdateModesParked`,
  `UpdateModesGatesCommitted(stage,modes,committed_w,committed_h,
  committed_mhz,active_paths)`, `UpdateModesAdapterTornDown`,
  `UpdateModesDrainTimeout`, `ParseDescription2` + `preferred`. New
  stages: `GATES_COMMITTED = 10`, `ADAPTER_TORN_DOWN = 11`.
- Add to the traced-install measurement: with both rates created and 120
  committed, `vgd-probe --target-mode 2560x1440@240` must now be REFUSED
  with `UpdateModesGatesCommitted` rather than forcing a modeset — so
  measure replace-vs-append by gating out the rate the OS is NOT running
  (create three rates, or commit the one you intend to keep first).

#### Second review pass (2026-07-31, build 17 still UNSIGNED)

Six findings, all verified against the code before being fixed; none was
misdiagnosed. One MAJOR, and it is about what the HOST can see.

- **A permanent refusal that reads as a transient one is a retry loop.**
  The `GATES_COMMITTED` refusal above settled as an ordinary
  `NotApplied`: sticky `err::UPDATE_FAILED`, which proto 0.5 documents as
  "the previous list is still in force and this request is fully
  retryable — resending it really does push again". So a retrying host
  queued a push, had it refused for the same reason, was told to retry,
  and never converged. The refusal is PERMANENT while that mode stays
  committed — retrying is the one response that cannot work — and the
  wire now says exactly that, in the reserved words the reply already had
  (still 40 bytes, no IOCTL value touched, `const_assert` unchanged):
  - `err::MODE_COMMITTED = -14` — appended, nothing renumbered. Appears as
    the `UPDATE_MODES` reply `result` AND as the monitor's sticky
    `MonitorStatus.last_error`, so a host that only polls `GET_STATUS`
    learns it too. `UPDATE_FAILED` keeps its meaning and is now documented
    as the RETRYABLE one.
  - `update_status::BLOCKED = 1 << 2` in `flags` (`reserved[2]`), read as
    `is_blocked()` / `worth_retrying()`.
  - `reserved[5]` (was "must be 0") = the blocking mode's index in the
    monitor's CREATE_MONITOR list, read as `blocking_mode_idx()`,
    sentinel `NO_MODE_INDEX` = `u32::MAX` — a bare 0 would read as "your
    first create-time mode", exactly the trap `NO_REJECTED_INDEX` exists
    for. An INDEX because the reply may never grow and one u32 is all
    there is; it costs no precision, because the superset IS the list the
    host sent at create.
  - FFI: `VgdUpdateModesReply.blocking_mode`, `VGD_UPDATE_BLOCKED`,
    `VGD_NO_MODE_INDEX` (safe to grow — nothing ships `vgd_update_modes`
    yet; the pinned submodule is still proto 0.4).
- **How the answer reaches a request at all.** The push is asynchronous —
  the IRP completes before the effects worker calls IddCx — so the only
  request that can be told about a refusal is a LATER one. The refusal is
  remembered in `Monitor.blocked` (`BlockedPush { targets, superset_idx,
  token }`) and a request resolving to that same list is answered from it
  with no effect emitted. `ModeUpdateResult::Blocked` is a third settle
  variant, and `PushOutcome::Blocked` its shell counterpart, so the
  distinction cannot be lost in the mapping (`retryable()` is the single
  definition behind both the ETW `retryable` field and the wire flag).
  **The block is evidence, so it expires like evidence**: it is honoured
  only while `modepush::committed_token()` — the `CommittedPaths`
  generation, bumped by every commit — still matches. Any modeset drops
  it and the next request pushes for real. A different selection also
  drops it (it is a different question, and may well keep the committed
  mode). Expiring early costs one push; expiring late would refuse a
  request that has become legal, which is the same bug in mirror image.
- **`CommittedPaths` is now a real seqlock.** The key WAS the sequence:
  publish payload, publish monitor handle, re-read the handle. Defeated by
  the commonest write there is — the same monitor committing again — which
  restores the same key while the payload changes underneath, pairing a
  size from one commit with a refresh rate from the next. A phantom mode is
  worse than none here, because it is compared against the superset to
  decide whether to refuse. Slots now carry an even/odd `version`
  (`fence(Release)` after the odd store, `fence(Acquire)` before the
  re-read), and `apply` visits every slot exactly once instead of clearing
  all keys and then filling the front.
- **Two writers can no longer interleave.** `apply` (modeset callback) and
  `forget` (effects worker) both rewrite the array. No lock is available on
  a callback frame, so they take a non-blocking token: the loser writes
  NOTHING and sets `contended`, and the holder then CLEARS the record
  rather than publish a mixture of two path sets. Clearing is the
  fail-open direction — "nothing committed" is exactly the pre-gate
  behaviour — so losing this race can only ever allow a push, never invent
  a refusal.
- **`active()` really can exceed the slots recorded now**, as it always
  claimed: `evt_commit_modes2` counts every active path and passes the
  total to `apply(paths, active_total)`. Deriving it from the truncated
  record made the documented case unreachable and understated the one
  number `UpdateModesGatesCommitted.active_paths` exists to report.
- **The fail-open eviction path traces.** `live_gate` returns a distinct
  `PushCommittedUnrecognised` verdict when the committed mode is not in the
  monitor's superset (a decoding mismatch), the push site emits
  `UpdateModesCommittedUnrecognised`, and `LiveGate::pushes()` keeps a call
  site from mistaking it for a refusal. Untraced, "the gate declined to
  fire because it could not read the commit" and "the gate had nothing to
  do" were the same silence — i.e. the whole feature could switch itself
  off invisibly.
- **`ParseDescription2.preferred` is the value REPORTED.** It was computed
  with `usize::MAX` for the trace and re-computed with `fill` for the OS,
  so on a truncated buffer the trace showed a number the OS never got.
  Computed once now, before the event; the event also carries `filled`.
- New/changed ETW (settle before signing — task #58's autologger keys on
  these): `UpdateModesGatesCommitted` gains `code`, `blocking_mode`,
  `commit_token`, `retryable`; `UpdateModesCommittedUnrecognised(stage,
  modes,superset,committed_w,committed_h,committed_mhz,active_paths)` is
  new; `UpdateModesDeferred`/`UpdateModesFailed` carry `retryable` from
  the one definition; `UpdateModesDenied` (dispatch layer) gains
  `blocked`, `blocking_mode`, `retryable` and reports stage
  `GATES_COMMITTED` for an answered-from-refusal reply;
  `ParseDescription2` gains `filled`. No new stage numbers.
- Regression tests that fail against the pre-fix behaviour (verified by
  reverting the settle arm and re-running):
  `dispatch::tests::a_retry_after_a_committed_mode_refusal_is_answered_not_re_pushed`
  (the wire: pre-fix gives `result == OK`, `PENDING`, and another queued
  push), `modepush::tests::a_refused_push_tells_the_host_it_is_permanent_
  and_which_mode_blocked_it`, and
  `modepush::tests::a_remembered_refusal_expires_when_anything_commits`.
  Plus `a_rewrite_that_restores_the_same_key_is_still_detected` and
  `a_lost_writer_clears_the_record_instead_of_mixing_two_commits` for the
  record's two races.
- Traced-install measurement, updated again: the refused push now prints
  its own diagnosis in `vgd-probe` ("refused PERMANENTLY … blocking mode:
  create-list index N"), so the run that gates out the committed rate is
  self-explaining rather than a silent no-op to be read out of ETW.

#### Third review pass (2026-07-31, build 17 still UNSIGNED — the last defect before signing)

Eight findings raised, six refuted, two fixed. One MAJOR, and it is the
same hole as the parked-patch split brain in its third guise: **a push
that CHANGED a published list committed nothing.**

- **A SUPERSEDED-BUT-SUCCESSFUL push made the RESCIND permanently
  unreachable, and reported `fully_in_force()` for a mode the monitor was
  not offering.** The interleaving is real and was reproduced against this
  branch's own core: the IOCTL holds the device lock only for `dispatch()`
  (control.rs), and the effects worker calls `IddCxMonitorUpdateModes2`
  with NO lock held, so request #2's whole dispatch lands inside push #1's
  DDI call. Push #1 publishes the runtime list and returns Applied on
  STATUS_SUCCESS — but `settle_modes` rejected it as stale, because the
  pending seq was now #2's, so `Monitor.target_modes` never learned what
  the OS took. Push #2 then settles NotApplied without publishing (the
  `tdr_duck_pending` arm, the NO_ADAPTER arm, or an OS refusal whose
  `restore_targets` puts push #1's list back — note the last one needs no
  TDR at all). Sticky `err::UPDATE_FAILED` tells the host to retry; its
  retry for the WIDER list now equals the stale `target_base()`, so
  `update_modes` short-circuits it as "already published": queued `None`,
  no effect, `result == OK`, no PENDING/PARTIAL flag ⇒
  `UpdateModesReply::fully_in_force()` TRUE while the OS and
  `MonitorRt.target_modes` hold the narrower list. Nothing resyncs it —
  `replug_ducked` and `duck_selected` both carry the RUNTIME list, so a
  duck/requalify/deadline cycle preserves the divergence and only a full
  replug-from-`DeviceState` heals it. **The doc at `settle_modes` named
  this interleaving and bounded its cost at "the durable subset lags what
  the OS holds until the next request" — but the next request is the
  rescind, which is exactly what the short-circuit swallows. The stated
  bound was not a bound; it is gone.**

  Fix, confined to the portable core (`session.rs` / the shell's one call
  site): the settle now carries the LIST the push put in front of the OS,
  not just the outcome, because a superseded push cannot read its own
  selection back out of `pending_targets`. A superseded settle whose
  outcome COMMITS writes `Monitor.target_modes` from it and reports
  `SettleOutcome::SupersededCommitted`; the newer update is untouched
  (pending kept, sticky error unwritten) and still decides on its own
  settle. A superseded settle that changed nothing still records nothing.
  `ModeUpdateResult::commits()` is the core's half of
  `PushOutcome::commits()`, so the two layers cannot drift.
- **…and the guard that keeps that from becoming its own bug.** Honouring
  superseded settles means the pending selection is no longer proof of
  either ownership or ordering, so both are explicit now:
  `Monitor.settled_seq`, a per-monitor high-water mark that STARTS at the
  table-wide `update_seq` standing at CREATE time. A settle is honoured
  only if strictly greater, which rejects (a) a re-ordered older settle
  putting an older list back over a newer push that already took, and (b)
  a destroyed session's settle reaching a replacement that reused its id —
  which the `pending_targets.seq == seq` check used to do by accident.
  Ids the table never issued are rejected too, and a committing settle
  refuses an empty list a third time (constraint 1: never a monitor with
  no targets).
- **MINOR — the `CommittedPaths` writer token had a loser window of its
  own.** `end_write` asked "was anyone turned away?" and the guard's drop
  released the token one statement later. A writer arriving between them
  was refused by a token nobody would look at again: its update was
  dropped, the record was left STALE (fail-CLOSED — a superseded committed
  mode is what makes `live_gate` refuse a push that should have gone
  through, contradicting the type's own "can only ever allow a push"
  invariant), and the `contended` flag it set survived with no holder to
  honour it, so the clear landed on the NEXT commit — a complete,
  uncontended one — instead. Token and flag are now ONE atomic word and
  the check is PART of the release (`WriteToken::drop` → `release()`,
  bounded CAS retry), so a writer is always turned away by a holder still
  in a position to clear. `begin_write` sets the flag with a CAS against
  `WRITING` rather than an unconditional `fetch_or`, and retries (bounded)
  when that CAS fails — which means the holder just released and the
  commit can be recorded properly instead of dropped.
- ETW: no renames, no new events. `UpdateModesSettleStale` gains
  `committed` (0/1) — without it `SupersededCommitted` and a settle that
  really decided nothing are the same line in a capture, which is the
  distinction the whole fix is about. Still settle names before signing
  (task #58's autologger).
- Regression tests that fail against the pre-fix behaviour (verified by
  reverting each fix and re-running):
  `session::tests::a_superseded_push_the_os_accepted_still_records_what_
  the_os_took` and `dispatch::tests::a_rescind_after_a_superseded_success_
  still_reaches_the_os` (the wire: pre-fix the rescind emits no effect and
  the reply says `fully_in_force()`),
  `session::tests::a_stale_settle_never_clobbers_a_newer_successful_push`,
  `session::tests::a_settle_can_never_empty_the_published_list`, and
  `modepush::tests::a_writer_turned_away_after_the_release_check_is_still_
  honoured` (pre-fix leaves the stale record standing).
  `a_second_request_while_a_push_is_outstanding_supersedes_and_stays_pending`
  and `a_stale_settle_that_changed_nothing_commits_nothing` were rewritten:
  they asserted the old "a superseded settle commits nothing" contract.

Host-side work remaining (LuminalShine, NOT done here): the pinned submodule
`src/drivers/luminal-display` is at da0349b = build 15 / proto 0.4, so it cannot
even see build 16 — any host work needs that pointer advanced first. Then the
call site at `virtual_display_vgd.cpp:361` (which today can only advertise the
base rate at CREATE time, and only if framegen is already known active) gains an
`UPDATE_MODES` path gated on `VGD_CAP_DYNAMIC_MODES`, degrading silently in the
style of the existing `proto_minor < 4` nits log. When it does, the one rule it
must honour is `worth_retrying()`: retry an `UPDATE_FAILED`, never a
`MODE_COMMITTED` — for the latter, either keep `blocking_mode` in the list or do
the `SetDisplayConfig` first.

### Build 23 — non-destructive transport containment (2026-08-03)

The Build-22 field trace proved that an optional direct-ring failure could
delete/reassign its IddCx swapchain thousands of times, exhaust D3D resources,
arm recovery on `E_OUTOFMEMORY`, and finally depart the only active monitor at
the ten-minute deadline. Build 23 makes each boundary explicit:

- A requested D3D12-fence ring that cannot provision its textures or fence
  downgrades once, in place, to the keyed-mutex ring. Proto 0.9 publishes the
  transport actually selected in `RingHeader.reserved0`; the layout does not
  grow and older drivers/hosts continue to read zero as keyed mutex.
- Any remaining non-device publish error opens a permanent drain-only circuit
  for that activation. The worker continues `ReleaseAndAcquireBuffer2` /
  `FinishedProcessingFrame`; it does not delete the swapchain or disturb the
  display. Only explicit DEVICE_REMOVED/HUNG/RESET enters reassignment/TDR.
- The device-duck terminal arm marks direct transport DEAD and keeps every
  monitor arrived. A recovery deadline can reduce performance, never topology.
- `RingTransportStageFailed` identifies texture-create, texture-share,
  keyed-mutex, fence-create, and fence-share failures with HRESULT, format,
  dimensions, generation, and session, so one ETL identifies the rejected API.

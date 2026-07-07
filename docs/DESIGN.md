# LuminalVGD — Architecture & Design

**Luminal Video Graphics Display Driver** for LuminalShine.
Rust UMDF IddCx driver (pf-vdisplay architecture) + ported SudoVDA session
semantics. **Direct-to-encoder is the primary capture mode; WGC is the
fallback.**

Status: pre-development design. Companion docs: `WGC-RELIABILITY.md`,
`FEATURE-MATRIX.md`, `../CLAUDE.md` (implementation plan).

---

## 1. Design thesis

Two proven codebases each solve half the problem:

- **punktfunk pf-vdisplay** (Rust, MIT/Apache-2.0) proves the *transport*:
  an IddCx driver that pushes finished frames straight into the host's
  encoder through a shared-memory/shared-texture ring — no Desktop
  Duplication, no WGC, no compositor round-trip.
- **SudoVDA** (C++, permissive) proves the *session model*: virtual monitors
  created and destroyed per streaming client via a control IOCTL, with exact
  modes, render-adapter selection, and a driver-side watchdog that reaps
  orphaned monitors.

LuminalVGD keeps pf-vdisplay's Rust core and transport, and **ports SudoVDA's
behaviors as specifications, not code** (different language; also keeps the
provenance story clean under AGPL-3.0 + NALA). See FEATURE-MATRIX.md for the
line-by-line disposition.

## 2. Capture mode ladder

```
1. VGD direct-to-encoder  (driver present + handshake OK)     ← primary
2. WGC                    (driver absent/incompatible/wedged) ← fallback
3. DXGI Desktop Duplication (WGC unavailable; last resort, optional)
```

Selection runs fresh at every session start (never cached across sessions —
Insider builds change the answer). Mid-session, failure only moves *down*
the ladder; recovery back to mode 1 happens at the next session.

## 3. Direct-to-encoder (primary mode)

### 3.1 Data path

```
DWM renders to VGD swapchain
  └─ IddCx AssignSwapChain → driver acquires buffer
       └─ driver copies/exports to cross-process shared texture ring
            └─ LuminalShine encoder (NVENC/AMF/QSV) consumes directly
```

- Ring of N (default 3) keyed-mutex shared D3D textures, allocated by the
  driver on the render adapter chosen at monitor creation.
- Metadata per slot (frame sequence, QPC present time, HDR10 metadata,
  dirty-rect summary if available) lives in a shared-memory header defined
  in `luminal-driver-proto` — the single ABI source of truth imported by
  BOTH the driver and LuminalShine. Never define the layout twice.
- Host signals consumption via keyed mutex release; driver recycles slots.
  Driver never blocks the IddCx swap-chain thread on the host: if the host
  stalls, the driver drops oldest and keeps sequence numbers monotonic so
  the host can detect the gap.

### 3.2 Control interface (ported SudoVDA semantics, LuminalVGD ABI)

Device interface GUID (new, LuminalVGD-owned): `LUMINAL_VGD_INTERFACE_GUID`.

| IOCTL | Purpose |
|---|---|
| `HANDSHAKE` | proto version + caps exchange; major mismatch → host refuses |
| `CREATE_MONITOR { w, h, hz, hdr, bit_depth, adapter_luid, session_id }` | per-client monitor with exact single-entry mode list |
| `DESTROY_MONITOR { session_id }` | explicit teardown at stream end |
| `PING { session_id }` | feeds the driver watchdog |
| `GET_STATUS` | monitor list, ring health, last error — for diagnostics |

SudoVDA behaviors preserved: max-monitors cap (default 10), watchdog
(default 3 s, 0 disables) that destroys monitors whose owner stopped
pinging (host crash → no zombie displays), SDR 8/10-bit and HDR 10/12-bit
depth options, render adapter selection with "largest VRAM" default when
unset. Configuration moves from SudoVDA's registry keys to explicit
`CREATE_MONITOR` parameters (registry fallback retained for global caps).

### 3.3 Recovery-first driver design (the WUDFHost-hang killer)

The reason LuminalVGD exists is SudoVDA wedging WUDFHost on current
release + Insider builds. Design rules:

1. **No unbounded waits anywhere in the driver.** Every keyed-mutex acquire,
   every event wait carries a timeout; timeout → drop frame, count it,
   continue. A hung host process must never hang the driver.
2. **TDR/adapter-reset survival:** on `DXGI_ERROR_DEVICE_REMOVED`/reset,
   tear down the D3D device and ring, re-create on the same adapter LUID,
   bump a `ring_generation` counter in shared memory so the host knows to
   re-map. Monitors stay attached; the stream resumes after one keyframe.
3. **IddCx callback hygiene:** callbacks return promptly; all D3D work on
   driver-owned worker threads; no locks held across IddCx calls.
4. **Watchdog self-report:** driver exposes `GET_STATUS` heartbeats so the
   host's recovery ladder can distinguish "driver alive, GPU resetting"
   from "driver gone" (different escalations — see WGC-RELIABILITY.md §4).

## 4. WGC fallback

Full treatment in `WGC-RELIABILITY.md`, including the three failure classes
behind WGC "getting stuck" on 24H2/Insider builds and the mitigation ladder.
Summary of the structural fix: **when the driver is present, WGC always
targets a LuminalVGD virtual display that is attached and active before the
capture session is created** — the 24H2 "DXGI fails because the target
display is off" class cannot occur against our own always-on virtual
output. Pure-fallback (driver absent) sessions targeting physical displays
follow the recovery ladder instead.

## 5. Host integration (LuminalShine)

- Lives under `src/platform/windows/` per the LuminalShine repo layout;
  the existing `virtual_display_backend` selector gains a `luminalvgd`
  value that becomes the default once this driver ships, superseding SudoVDA.
- `CaptureBackend` abstraction: `name/start/next_frame/stop/health`;
  encoder consumes GPU-resident `Frame` objects and is backend-agnostic.
- Probe order at session start: enumerate interface GUID → open → handshake
  → `CREATE_MONITOR` → map ring. Any failure falls through to WGC with a
  logged reason code.
- Frame-generation-aware refresh doubling (planned-scope item): host
  requests 2× client refresh at `CREATE_MONITOR` when frame-gen is active;
  driver just honors the mode — policy stays in the host.

## 6. Packaging & signing

- INF: `luminalvgd.inf`, hardware ID `root\luminal_vgd`, device description
  "Luminal Video Graphics Display", provider "NortheBridge Foundation".
- OV certificate signing: sign driver DLL + catalog (`inf2cat` → `signtool
  /fd sha256 /tr <RFC3161> /td sha256`); installer seeds
  **LocalMachine\TrustedPublisher only** (OV already chains to a trusted
  root — never touch the Root store); clear the FORCE_INTEGRITY PE bit
  after link (windows-drivers-rs sets it; non-Microsoft signatures fail it).
- Install: OS floor check (Windows 11; 24H2 required for full HDR — mirror
  SudoVDA's documented constraint), create root-enumerated devnode, `pnputil
  /add-driver /install`. Uninstall reverses all three.
- Future: EV cert + Microsoft attestation signing drops the TrustedPublisher
  step; architecture unchanged.

## 7. Licensing & provenance rules (binding for contributors)

- Repo license: AGPL-3.0 with NALA commercial option (see LICENSING.md).
  CLA required so the Foundation can license under both.
- pf-vdisplay-derived Rust code: permitted (MIT OR Apache-2.0 → AGPL-3.0),
  retain notices in THIRD-PARTY-NOTICES.md.
- SudoVDA: port **behavior only** into Rust. If any C++ is ever translated
  closely enough to be a derivative, first verify the LICENSE file in the
  SudoVDA repo matches the README's permissive statement, then record it in
  THIRD-PARTY-NOTICES.md. Default stance: clean-room the semantics from
  this design doc.
- Microsoft IddSampleDriver lineage (via SudoVDA/MTT): MIT — reference only.

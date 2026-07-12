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
Insider builds change the answer). Mid-session, failure moves *down* the
ladder immediately — and the host then works to move back **up to mode 1 as
soon as possible**, within the same session (see §2.1).

### 2.1 Seamless fallback & mid-session restore (product requirement, 2026-07)

> This supersedes the original rule that recovery to direct-to-encoder
> waits for the next session.

When direct-to-encoder fails mid-stream (driver heartbeat stale, ring
`DEAD`/`REBUILDING`, frame starvation):

- **The transition is seamless.** WGC takes over between frames against
  the still-attached LuminalVGD virtual display, one keyframe is forced,
  and the client never sees an interruption.
- **The transition is silent at the OS level.** No Windows toast or any
  other OS-surface notification is raised — the host-side notice channel
  structurally has no OS-toast variant. LuminalShine surfaces the state in
  its own UI and structured logs only, with copy that tells the user
  LuminalShine has temporarily fallen back to Windows Graphics Capture and
  *"will try restoring direct encoding as soon as possible."*
- **Restore is active, not next-session.** The host probes the driver
  (handshake + ring health) on an exponential backoff (1 s → 30 s cap).
  The moment the driver is healthy — e.g. the ring generation bumps after
  a TDR rebuild, or the driver was reinstalled — the encoder swaps back to
  the ring, forces a keyframe, and keeps the WGC session warm until direct
  encoding has proven stable (default 120 frames), so a relapse is another
  seamless swap rather than a cold start.

Implementation: `luminal-vgd-host::controller` (state machine, fully
unit-tested) + `notice` (copy and channel).

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
| `CREATE_MONITOR { session_id, display_id, modes[≤4], hdr, bit_depth, adapter_luid, lease_timeout_ms, physical_mm, … }` | per-client monitor; `modes[0]` preferred |
| `DESTROY_MONITOR { session_id }` | explicit teardown at stream end |
| `PING { session_id }` | feeds the per-lease watchdog |
| `QUERY_LEASE { session_id }` | identity, connector, remaining lease time |
| `SET_RENDER_ADAPTER { luid }` | device-wide preference for unset-adapter creates |
| `SET_PERMANENT_POOL` / `QUERY_PERMANENT_POOL` | always-on display pool (see §3.2.2) |
| `GET_STATUS` | monitor list, ring health, last error — for diagnostics |

SudoVDA behaviors preserved: max-monitors cap (default 10), PING-fed
watchdog that destroys monitors whose owner stopped pinging (host crash →
no zombie displays), SDR 8/10-bit and HDR 10/12-bit depth options, render
adapter selection with "largest VRAM" default when unset. Configuration
moves from SudoVDA's registry keys to explicit `CREATE_MONITOR` parameters
(registry fallback retained for global caps).

#### 3.2.1 Display identity vs. lease (libvirtualdisplay fold-in, proto v0.3)

`session_id` is a *lease* — it lives exactly as long as one stream.
`display_id` is the monitor's *identity*: it determines the EDID product
code, serial, container GUID, and (via driver-persisted connector
reservations) the IddCx connector. A client that reconnects with the same
`display_id` is, to Windows, the same monitor — resolution, HDR state, and
desktop position are restored by the OS instead of re-learned. Hosts that
don't want that pass `EPHEMERAL_IDENTITY`. Lease timeouts are per-monitor
(3 s–300 s, `USE_DEFAULT` defers to the SudoVDA-style registry default,
`DISABLED` for pool displays). Reserved identity ranges (permanent
`0x7000…`, ephemeral `0xE000…`) are refused from the wire.

#### 3.2.2 Permanent display pool

Up to 4 identical always-on displays that exist outside any stream,
configured via `SET_PERMANENT_POOL`, persisted (with connector
reservations) in a schema-versioned registry blob, and recreated by the
driver at device start. This replaces the SudoVDA `option.txt` use case
the matrix dropped, and backs LuminalShine's keep-display-while-paused
behavior with a first-class mechanism.

#### 3.2.3 Hardware cursor plane (`caps::HW_CURSOR`)

The driver registers for IddCx hardware-cursor callbacks (alpha + masked,
up to 256×256) and republishes shape/position into a per-monitor shared
cursor section (`CursorHeader` + pixel buffer; shape changes bump a
generation counter, position updates are header-only). The host forwards
cursor state to the client for client-side rendering — no cursor baked
into encoded frames, no added latency on cursor motion. IddCx wiring lands
with the phase-2 shell; the ABI ships in proto v0.3.

#### 3.2.4 EDID

Generated per monitor (256 bytes): base block carries identity (product
code from connector/pool index, serial from `display_id`), the preferred
detailed timing, and real physical dimensions (mm, from the create request
— drives correct DPI scaling); the CTA-861 extension carries HDR static
metadata (PQ EOTF, ST 2086 luminance) and BT.2020 colorimetry, which is
what makes the Windows HDR toggle dependable on a virtual display.

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
5. **Teardown deadline budgeting** (libvirtualdisplay pattern): monitor
   departure shares one deadline across cursor/swapchain worker stops
   (≈500 ms each, remaining-budget computed per step), and a failed
   departure is tracked (`pending`) rather than retried inline — a wedged
   worker can never extend teardown unboundedly or dangle a callback.
6. **Postmortem-first tracing:** the phase-2 shell registers an ETW
   TraceLogging provider and builds WPP with the Inflight Trace Recorder,
   so a wedged WUDFHost's recent trace ring is recoverable from a debugger
   (`!wdfkd.wdflogdump`) — evidence for exactly the hang class this driver
   exists to kill.

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
- **Control-surface ACL (release blocker):** the control device interface
  gets a strict SDDL (SYSTEM + Administrators only; LuminalShine's service
  runs as SYSTEM) — an unprivileged process must not be able to create,
  destroy, or lease monitors. A permissive ACL in a shipped package blocks
  the release (rule adopted from libvirtualdisplay's release-validation
  gates, along with functional install/upgrade/identity-retention/lease-
  expiry validation per release).
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

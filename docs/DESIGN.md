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

- Ring of N (default 3) shared D3D textures, allocated by the driver on the
  render adapter chosen at monitor creation. Proto 0.10 hosts request the
  D3D12-openable/timeline-fence transport as an immutable requirement;
  provisioning failure makes that optional ring DEAD while IddCx continues
  draining. Older hosts retain the keyed-mutex contract.
- Metadata per slot (frame sequence, QPC present time, HDR10 metadata,
  dirty-rect summary if available) lives in a shared-memory header defined
  in `luminal-driver-proto` — the single ABI source of truth imported by
  BOTH the driver and LuminalShine. Never define the layout twice.
- Host signals consumption via the shared slot state after its bounded GPU
  copy completes (and by keyed-mutex release for legacy sessions); the driver
  recycles slots.
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
| `UPDATE_MODES { session_id, modes[≤4] }` | re-publish which of a LIVE monitor's create-time modes it offers (see §3.2.5); `caps::DYNAMIC_MODES` |
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

#### 3.2.5 Dynamic mode lists (`caps::DYNAMIC_MODES`, proto 0.5, build 17)

Before build 17 the set of modes a monitor offered was fixed at
`CREATE_MONITOR`: the only way to change it was `DESTROY_MONITOR` +
`CREATE_MONITOR`, i.e. a monitor cycle — which broadcasts
`DBT_DEVNODES_CHANGED` (the documented GTA V Enhanced killer, an
uncatchable `0xC000041D` in its own handler) and which amplified the
2026-07-30 machine-wide wedge. The motivating case: a client streams
"Desktop" over Moonlight and only *then* launches a frame-generation
title whose doubled rate should become the one on offer.

**The model, established from the IddCx 1.10 headers, the 1.11 update
page, the `IddCxMonitorUpdateModes2` reference and the (1.10, MIT)
VirtualDrivers/Virtual-Display-Driver source, which calls UpdateModes
zero times:**

- `IDARG_IN_UPDATEMODES2` (IddCx.h:3586) carries only
  `{ Reason, TargetModeCount, pTargetModes }` — **TARGET** modes.
- **No DDI in IddCx 1.10 or 1.11 replaces an arrived monitor's
  DESCRIPTION.** Its monitor-mode set is fixed at `IddCxMonitorCreate`
  (verified against the whole 36-entry function table). Changing it means
  departure + recreate.
- `IDDCX_ADAPTER_FLAGS_REMOTE_ALL_TARGET_MODES_MONITOR_COMPATIBLE`, which
  would skip the intersection, is documented remote-drivers-only; a
  console-session driver cannot opt in (setting `REMOTE_SESSION_DRIVER`
  fails adapter init).
- Windows selects from the **intersection** of the monitor-mode list and
  the target-mode list.

So: the driver advertises a static **superset** of monitor modes at
creation (the `CREATE_MONITOR` list, carried by the EDID and served from
`EvtIddCxParseMonitorDescription2`), and uses
`IddCxMonitorUpdateModes2` to publish the currently valid **target
subset**. That works with no departure provided every published target has
a compatible entry in the superset. It can gate and steer within the
superset; it can never enlarge it. **Hosts must create a monitor with
every mode they might later want** — e.g. both the base rate and the
framegen-doubled rate.

Build 17's first cut implemented an additive merge onto what it treated as
the monitor list. That was the wrong list and the wrong operation.

`UPDATE_MODES` binds `IddCxMonitorUpdateModes2` (function-table index 34).
**Index 6 (`IddCxMonitorUpdateModes`) is not bound at all**: per its
reference page, "drivers reporting `IDDCX_ADAPTER_FLAGS_CAN_PROCESS_FP16`
can only call `IddCxMonitorUpdateModes2`; calling
`IddCxMonitorUpdateModes` is an error", and CAN_PROCESS_FP16 is the only
adapter flag we set (the HDR10 contract).

The semantics are **replace-target-modes**, and every clause is a safety
property rather than a convenience:

- The request is the complete target list to publish. Each entry is
  checked against the create-time superset; an entry with no counterpart
  there is REJECTED WITH DETAIL (count + index of the first) rather than
  published — it could never surface through the intersection anyway.
- **`targets ⊆ superset` is the invariant.** Every list the driver ever
  hands the OS is a subset of a description the monitor really has, so
  the intersection is non-empty and every mode in it is activatable.
- **The target list can be replaced but never emptied**: IddCx.h:3594
  states `TargetModeCount` "cannot be zero". `mode_count == 0` and a
  request whose entries are all outside the superset are both refused with
  `err::BAD_MODE`, changing nothing. A refused REQUEST is never a failed
  session.
- The monitor-mode list is untouched, so `modes[0]` stays the EDID's
  preferred detailed timing for the life of the monitor. Every monitor
  mode is reported `IDDCX_MONITOR_MODE_ORIGIN_MONITORDESCRIPTOR`, which is
  now simply true. **`PreferredMonitorModeIdx` is not a constant 0,
  though**: Windows offers the INTERSECTION of the two lists, so once a
  gate excludes `modes[0]` an index of 0 names a mode the OS cannot
  activate — its default choice unreachable, with nothing in a trace
  saying why. The parse DDIs report the first monitor mode that is
  actually being offered (`modepush::preferred_monitor_mode_idx`,
  computed against what was written into the OS's buffer), and it rides
  `ParseDescription2` as `preferred`.
- Gating is REVERSIBLE — a later request republishes the wider subset —
  which an append-only merge structurally could not express.
- **`UPDATE_MODES` steers what the OS MAY select; it never evicts what the
  OS HAS selected.** Publishing a subset without the committed mode forces
  Windows to re-select on what, under
  `virtual_display_layout=exclusive`, is the only active display — a
  modeset in the middle of the stream this feature exists to avoid
  disturbing. `EvtIddCxAdapterCommitModes2` records the committed path per
  monitor (below), and a push that would gate the committed mode out is
  refused at `modepush::stage::GATES_COMMITTED`. A refused PUSH is not a
  refused session: the published list stays in force, and the monitor and
  the stream carry on.

  **This refusal is PERMANENT while that mode stays committed — retrying
  cannot clear it, and the wire says so.** `result` and the sticky
  `MonitorStatus.last_error` are `err::MODE_COMMITTED` (-14), distinct from
  the *retryable* `err::UPDATE_FAILED` (-13); `reserved[2]` carries
  `update_status::BLOCKED`, read as `reply.is_blocked()` /
  `reply.worth_retrying()`; and `reserved[5]` carries the blocking mode's
  index into the monitor's `CREATE_MONITOR` list (`blocking_mode_idx()`,
  sentinel `NO_MODE_INDEX`) so the host can name the mode rather than guess.
  Because the push is asynchronous only a LATER request can be told, so the
  refusal is remembered in `Monitor.blocked` and a request resolving to the
  same list is answered from it with no effect emitted; it expires on any
  change to `modepush::committed_token()` or any different selection.

  A host that wants the display on a DIFFERENT mode performs the
  modeset itself (`SetDisplayConfig` — what the display helper already
  drives for topology) and gates afterwards, at which point the committed
  mode is one it is keeping.

**The committed path.** `EvtIddCxAdapterCommitModes2` is handed the
complete `IDDCX_PATH2` set for the adapter; build 17 first read only
`PathCount` and threw the rest away, which is why the driver could
neither detect nor trace the case above. The ACTIVE paths are now captured
into `modepush::CommittedPaths`, a lock-free fixed-slot record keyed by
`IDDCX_MONITOR`: the writer is a CALLBACK FRAME inside a modeset
transaction, so it takes no lock, allocates nothing, and does a bounded
number of atomic stores; the reader (the effects worker, immediately
before its DDI call) does a bounded seqlock-style read. The record is
REPLACED on every commit — a monitor absent from the path set has nothing
committed — and forgotten explicitly on departure, since an
`IDDCX_MONITOR` handle can be reissued. Two writers cannot interleave: the
writer token and the "somebody was turned away" flag are ONE atomic word,
and the flag is checked as PART of handing the token back, so a writer that
loses is always turned away by a holder still in a position to act on it.
The holder then CLEARS the record rather than serve a path set that may be
missing a write — the fail-open direction, so losing that race can allow a
push but never invent a refusal. (Two separate flags left a window between
"was anyone turned away?" and the release itself: a writer arriving there
had its update dropped, left the record STALE, and left a flag with no
holder to honour it, which then erased the next complete commit instead.)
Every path is traced
(`CommitModes2Path`) with its committed mode whether active or not, which
is what makes "which mode is this display running" answerable from a
capture at all. The gate FAILS OPEN: a committed mode the monitor's
superset does not describe cannot be reasoned about, so the push proceeds
exactly as it did before and the trace says so — a gate that fired on an
unrecognised commit would silently turn the feature off.

Application rules (DESIGN.md §3.3): the IOCTL only validates and selects,
then queues an `Effect::UpdateModes`; the OS call happens on the effects
worker with **no lock held**, because `IddCxMonitorUpdateModes2` makes the
OS re-enter `EvtIddCxMonitorQueryTargetModes2` /
`EvtIddCxParseMonitorDescription2` synchronously on the calling thread and
`std::sync::Mutex` is not reentrant. The new target list is published into
the monitor runtime *before* the call (the re-entrant query must see it)
and restored on failure. A failed update degrades to "target modes
unchanged, carry on" — never a departure, never an empty target list,
never a refused session — and surfaces as `err::UPDATE_FAILED` in the
monitor's sticky `GET_STATUS` last error plus an ETW event carrying stage
and status. `Reason` is `IDDCX_UPDATE_REASON_CONFIGURATION_CONSTRAINTS`
(IddCx.h:327).

A final device exit runs on a WDF power callback, CONCURRENTLY with the
effects worker, and destroys the adapter's monitor objects — so a push
must never straddle one. Checking `Shell::adapter()` before the DDI call
(build 17) left the whole interval to the call unguarded. The two sides
now handshake: the push marks itself in flight, fences, and reads handle
and epoch in ONE acquisition (read apart, a `clear_adapter` between them
returns a live handle with the epoch that already invalidated it);
`EvtDeviceD0Exit(D3Final)` clears the adapter, fences, and then drains
in-flight pushes on a 500 ms deadline inside the multi-second worker drain
it already performs. With SeqCst fences on both sides at least one side
observes the other, so either the push sees the cleared adapter and never
calls, or the teardown waits for the push. The epoch is re-checked once
more after the new list is published and before the call; that path
restores the runtime list and defers
(`modepush::stage::ADAPTER_TORN_DOWN`).

**Commit ordering — nothing is committed until the OS takes it.** There
are three copies of the published list (the session table's durable one,
the shell's runtime one, and a TDR-parked spec) and the reply is written
on a different thread from the push, so the ordering is the whole
correctness argument:

- `SessionTable::update_modes` validates and selects but writes the result
  to `Monitor.pending_targets`, **not** `Monitor.target_modes`. The effect
  carries the selection plus a table-wide monotonic `update_seq`.
- `monitors::update_modes` (effects worker) is the only caller of
  `IddCxMonitorUpdateModes2` and owes the table exactly one
  `settle_modes(session_id, update_seq, …)`. The outcome→settle mapping is
  `modepush::PushOutcome::settle_result`, and there is exactly one of it:
  **anything that CHANGED a published list commits; anything that changed
  nothing does not.**
- A push that FAILED (the OS refused; the runtime list is rolled back) or
  was DEFERRED (a TDR duck in flight, the adapter torn down under a
  D3Final) settles `NotApplied`: the pending selection is discarded, every
  copy keeps the pre-update list, and the monitor's sticky last error
  becomes `err::UPDATE_FAILED`. The **next identical request therefore
  selects, queues and pushes for real** — a retry is a retry, not a no-op.
- A push REFUSED because it would gate out the OS-committed mode settles
  `ModeUpdateResult::Blocked` — the third outcome, distinct from both of
  the above. Nothing is published and nothing changes, but unlike
  `NotApplied` the refusal is NOT retryable: it holds for as long as that
  mode stays committed. It is remembered in `Monitor.blocked` and a later
  request resolving to the same selection is answered from that memory with
  no effect emitted, carrying `err::MODE_COMMITTED` and
  `update_status::BLOCKED`. The memory expires on any change to
  `modepush::committed_token()` or any different selection — expiring early
  costs one push, expiring late would refuse a request that had become
  legal.
- A push landing on a TDR-PARKED session patches the parked spec — there
  is no monitor object to call, and the re-arrival is that monitor's only
  publication mechanism — and therefore COMMITS
  (`PushOutcome::AppliedParked`, a separate variant only so the
  replace-vs-append measurement on `UpdateModesApplied` is not polluted by
  a call-less application). Build 17 patched the spec and settled
  `NotApplied`, which is a permanent split brain: the re-arrived monitor
  published the new subset while the durable list kept the old one, and
  since `update_modes` short-circuits a request matching the durable list,
  the host's RESCIND was answered "already published, nothing to do" —
  no effect, `OK`, and the gate stuck for the life of the session.
  `commits()` is now both the authorisation to write the parked spec and
  the settle decision, so the two halves cannot drift apart again.
- **A SUPERSEDED push still reports what the OS took.** `dispatch` holds
  the device lock only for its own duration and the effects worker calls
  `IddCxMonitorUpdateModes2` with no lock held, so request #2's entire
  dispatch can land inside push #1's DDI call — at which point push #1
  returns success with its `update_seq` no longer outstanding. A settle in
  that position is not current, but it is the only thing that knows the OS
  accepted its list, so when its outcome COMMITS it writes
  `Monitor.target_modes` anyway (`SettleOutcome::SupersededCommitted`).
  The newer update is untouched: its pending selection stays, its sticky
  error stays unwritten, and it still decides on its own settle. A
  superseded settle whose push changed nothing records nothing.

  Build 17 called every superseded settle "commits nothing", and bounded
  the cost at "the durable subset lags what the OS holds until the next
  request". That bound did not hold, because the next request is the
  RESCIND and `update_modes` short-circuits a request matching the durable
  list: after a superseded-but-successful push #1 and a push #2 that did
  not apply (a duck in flight, a torn-down adapter, or an OS refusal whose
  rollback restores push #1's list), the host was told to retry, its retry
  matched the stale durable list, and it was answered "already published"
  — no effect, `result == OK`, no PENDING flag, i.e.
  `UpdateModesReply::fully_in_force()` TRUE for a list the monitor was not
  offering. Nothing resynced it: `replug_ducked` and `duck_selected` both
  carry the RUNTIME list, so a duck/requalify cycle preserved the
  divergence and only a full replug from `DeviceState` healed it. The
  rescind was unreachable for the life of the session. This was the same
  hole as the parked-patch split brain, in its third guise: a push that
  changed a published list committed nothing.
- **A settle is honoured only if it is NEWER than every settle already
  honoured** (`Monitor.settled_seq`, a per-monitor high-water mark that
  starts at the table-wide counter's value at CREATE time). That is what
  keeps the rule above from becoming its own bug — a re-ordered older
  settle can never put an older list back over a newer push that already
  took — and it is also what stops a destroyed session's settle reaching a
  replacement that reused its id, which the pending-selection check used to
  do by accident. A settle for a session that is gone, for an id below the
  mark, or for an id the table never issued decides nothing; either way
  both lists are non-empty subsets of the same frozen superset, so whichever
  is in force is fully activatable.

Committing at IOCTL time instead — as build 17 first shipped — made the
durable state assert a publish the OS had refused, and turned the
identical retry into a silent no-op that returned `OK`: the caller was
told it succeeded, the monitor offered the old list, and there was no way
back.

**Partial application is reported, not swallowed.** The selection keeps
every requested mode the monitor really has and counts the rest
(`accepted` / `rejected` / `first_rejected`). The reply carries them plus
a flags word in `UpdateModesReply.reserved` — the struct may never grow,
both sides length-check it — where `update_status::PARTIAL` means "some
requested modes are not in the monitor's create-time list and can never be
offered", `update_status::PENDING` means "queued at the OS, not in
force yet", and `update_status::BLOCKED` means "refused because the
selection would gate out the mode the OS has committed — permanent while
that mode stays committed, so do NOT retry" (paired with
`result == err::MODE_COMMITTED` and, in `reserved[5]`, the blocking mode's
index into the create-time list; see the committed-path section above).
Partial is success with detail: everything that exists is
published and the session is never failed (constraint 1). `result == OK`
with neither flag set is the only shape that means "everything you asked
for is offered right now" (`UpdateModesReply::fully_in_force`).

**Open empirical question (build 17's first traced install must answer
it), and why it cannot break anything.** Nothing in the 1.10 headers or on
the reference page says whether `IddCxMonitorUpdateModes2` REPLACES the
monitor's target list or APPENDS to it — the Remarks say only "update the
mode list previously reported for a monitor". The code assumes REPLACE and
says so at `shell::monitors::push_targets`, but it is safe either way
*because* of `targets ⊆ superset`: under replace the OS holds `targets`,
under append `previous ∪ targets`, and under a re-solicit exactly
`targets` — in all three a non-empty subset of the frozen superset, so
nothing unactivatable and no monitor left with no targets. Only
effectiveness differs (an append would fail to remove a rate, and the host
can see that). **How to tell:** publish a strict subset, then read
`UpdateModesApplied(modes, superset)`, whether `QueryTargetModes2`
reappears with the pushed count, and finally how many rates Display
Settings offers — `published` ⇒ replace or re-query, `superset` ⇒ append
or no re-solicit. `vgd-probe --target-mode` exercises it standalone.
Likewise undocumented: whether an update broadcasts a devnode change. Do
not assert either way in code or docs until measured.

#### MEASURED 2026-07-31 (build 17, Insider 29617, RTX 5080) — three facts

Run: `vgd-probe 3840x1600@120 3840x1600@60 --hold 90 --target-mode
3840x1600@120 --target-after 20`, watching Display Settings.

1. **A console-session driver CAN advertise a multi-mode superset, and
   Windows presents all of it.** Both 60 Hz and 120 Hz appeared in the
   refresh dropdown at CREATE_MONITOR, before any update. This is the
   precondition the whole superset/subset model rests on, and it holds —
   `REMOTE_ALL_TARGET_MODES_MONITOR_COMPATIBLE` being remote-only does NOT
   stop a console driver publishing several monitor modes; it only stops it
   publishing TARGET modes that no monitor mode covers.
2. **`IddCxMonitorUpdateModes2` REPLACES the target list.** Publishing
   `{120}` left 120 as the only rate offered; an append would have kept 60.
   This settles the question the 1.10/1.11 headers, the Learn reference and
   the VirtualDrivers reference driver all leave open.
3. **An update costs a swapchain reassign and a visible brief blank —
   EVEN when the committed mode is preserved in the published set.** The
   ring generation went 1 → 3 (unassign + assign) ~2 s after the push, and
   the screen blanked momentarily. It is a MODESET, not a monitor cycle:
   no departure, no arrival, so no `DBT_DEVNODES_CHANGED` and no exposure
   to the GTA V device-change fault. But it is not free.

**Consequence for callers.** `UPDATE_MODES` is not a per-session or
speculative call. Where the need is "this display should be able to run at
2x for frame generation", advertise both rates at CREATE_MONITOR and change
nothing afterwards — zero display events. Reach for `UPDATE_MODES` only when
a mode genuinely must stop being offered and a brief blank is acceptable.

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

   *Build 16 note — "duck the device, not the display".* Builds 14/15
   deviated from this rule: they DEPARTED every monitor on device removal,
   to keep a failing OS TDR recovery from waiting on the indirect display
   path. The 2026-07-30 incident showed the cost — under
   `virtual_display_layout=exclusive` the departure took the active display
   count to zero, dwm.exe declared a black screen 131 ms later,
   `QueryDisplayConfig` went to zero paths and then `ERROR_NOT_SUPPORTED`
   permanently, and Windows never logged an Event 4101 (so the OS recovery
   cycle the departure existed to unblock never even ran). Build 16
   restores the rule as written: the D3D device and swapchain go, the ring
   goes REBUILDING with a device-independent heartbeat, and **the IddCx
   monitor stays ARRIVED** so Windows keeps a display path to compose onto.
   Build 23 removes the terminal departure as well: if requalification or
   the recovery budget is exhausted, direct transport becomes DEAD while
   the monitor remains arrived. The host can continue HDR-capable fallback
   capture against that stable desktop and a later session can build a fresh
   ring. The builds-14/15 behaviour stays selectable only through the
   explicit `LuminalVgdTdrDuckMode` REG_DWORD (see §6).

   Contract basis for keeping the monitor arrived: IddCx already drives
   monitors into "arrived with no swapchain and no device" as routine
   operation — `EvtIddCxMonitorUnassignSwapChain` leaves the monitor
   arrived, and the OS unassigns ~10 ms after every activation. Nothing in
   IddCx ties a D3D device to monitor arrival; `IddCxSwapChainSetDevice`
   scopes the device to the *swapchain*.
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
- **Registry knobs** (devnode `Device Parameters` key, i.e.
  `HKLM\SYSTEM\CurrentControlSet\Enum\ROOT\DISPLAY\000x\Device Parameters`
  — survives package updates, cleared only by `pnputil /remove-device`):
  - `LuminalVgdTdrDuckMode` (REG_DWORD): `0`/absent = duck the device, keep
    the display (§3.3 rule 2, the default); `1` = legacy builds-14/15
    display duck-out. Read at device add, so `pnputil /restart-device`
    applies it — no reinstall. Deliberately NOT seeded by the INF's AddReg:
    HKR there rewrites on every package update and would silently stamp an
    operator's override back to the default mid-upgrade.
  - `LuminalVgdState` (REG_BINARY): identity reservations + permanent pool.
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

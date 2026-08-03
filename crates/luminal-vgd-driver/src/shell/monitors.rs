// SPDX-License-Identifier: AGPL-3.0-only
//! Monitor plug/unplug via IddCx and the mode-list callbacks.
//!
//! Identity matching: EvtIddCxParseMonitorDescription receives only the
//! EDID (no monitor object), so sessions are found by the identity octets
//! our generator embeds — bytes 8..16 (vendor id, product code, serial).

use core::mem::{size_of, zeroed};
use core::sync::atomic::{fence, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use wdk_sys::{NTSTATUS, STATUS_INVALID_PARAMETER, STATUS_SUCCESS};

use super::bindings::{self, ffi};
use super::PROVIDER;
use super::{MonitorRt, OsHandle, Shell};
// `COMMITTED` is the paths the OS last committed, per monitor object:
// written from `evt_commit_modes2` (a CALLBACK FRAME — lock-free,
// allocation-free, fixed work) and read by the effects worker immediately
// before an `IddCxMonitorUpdateModes2`. A process global like the rest of
// the shell's state (one root-enumerated devnode), and deliberately not a
// field of `Shell`: the modeset callback that writes it must not depend on
// `Shell::get()` having run. It is DEFINED in `modepush`, not here, because
// `dispatch` reads its generation on every `UPDATE_MODES` — the validity
// token for a remembered refusal — and nothing below the shell line may
// reach up into the shell.
use crate::modepush::{
    self, stage, CommittedMode, LiveGate, PushOutcome, COMMITTED, MAX_COMMITTED_PATHS,
};
use luminal_vgd_core::modes::Mode;

/// Deterministic container GUID for a display identity: same display_id
/// → same GUID across reconnects and reboots (identity retention).
fn container_guid(display_id: u64) -> ffi::GUID {
    ffi::GUID {
        Data1: (display_id >> 32) as u32,
        Data2: (display_id >> 16) as u16,
        Data3: display_id as u16,
        // "LuminalV" — a fixed, driver-owned node so these GUIDs can never
        // collide with anything not created by LuminalVGD.
        Data4: *b"LuminalV",
    }
}

/// Plug one monitor: IddCxMonitorCreate + IddCxMonitorArrival.
/// Called with no locks held (only takes the monitors map lock briefly).
#[allow(clippy::too_many_arguments)]
pub fn plug(
    session_id: u64,
    display_id: u64,
    connector_index: u32,
    monitor_modes: Vec<Mode>,
    target_modes: Vec<Mode>,
    adapter_luid: u64,
    ring_slots: u32,
    transport_flags: u32,
    edid: Box<[u8; 256]>,
) {
    let shell = Shell::get();
    let Some(adapter) = shell.adapter() else {
        tracelogging::write_event!(PROVIDER, "PlugBeforeAdapterReady", level(Error));
        return;
    };

    // Purge any parked (ducked) copy of this session FIRST — before the
    // FrameRing below creates its shared section. A stale parked entry
    // still holds the old section alive under the same name, and
    // CreateFileMappingW would silently reopen it instead of creating a
    // fresh one — two ring Arcs aliasing one section. Dead-mark so the
    // host stops waiting on the stale mapping.
    {
        let mut ducked = shell.ducked.lock().unwrap();
        if let Some(pos) = ducked.iter().position(|d| d.session_id == session_id) {
            let d = ducked.remove(pos);
            drop(ducked);
            super::mark_ring_dead_arc(&d.ring);
        }
    }

    unsafe {
        let mut info: ffi::IDDCX_MONITOR_INFO = zeroed();
        info.Size = size_of::<ffi::IDDCX_MONITOR_INFO>() as u32;
        info.MonitorType = ffi::DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY_DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_WIRED;
        info.ConnectorIndex = connector_index;
        info.MonitorContainerId = container_guid(display_id);
        info.MonitorDescription.Size = size_of::<ffi::IDDCX_MONITOR_DESCRIPTION>() as u32;
        info.MonitorDescription.Type = ffi::IDDCX_MONITOR_DESCRIPTION_TYPE_IDDCX_MONITOR_DESCRIPTION_TYPE_EDID;
        info.MonitorDescription.DataSize = 256;
        info.MonitorDescription.pData = edid.as_ptr().cast::<core::ffi::c_void>().cast_mut();

        let mut in_args: ffi::IDARG_IN_MONITORCREATE = zeroed();
        in_args.pMonitorInfo = &mut info;
        let mut out_args: ffi::IDARG_OUT_MONITORCREATE = zeroed();
        let status = bindings::monitor_create(adapter.0.cast(), &in_args, &mut out_args);
        if status != STATUS_SUCCESS {
            tracelogging::write_event!(
                PROVIDER,
                "MonitorCreateFailed",
                level(Error),
                u64("session", &session_id),
                i32("status", &status)
            );
            return;
        }
        let monitor = out_args.MonitorObject;

        // The ring section exists from plug time (state REBUILDING, no
        // frames yet), before the monitor arrives. It becomes ACTIVE only
        // after the first copied/fenced slot is published. Plug runs on the effects
        // worker shortly after the CREATE_MONITOR reply completes; the
        // host's ring open already retries on its own timeout budget.
        let ring = std::sync::Arc::new(super::swapchain::RingHandle::new(
            super::swapchain::FrameRing::new(session_id, ring_slots, transport_flags),
        ));
        // Every mode this monitor object is created with is
        // descriptor-origin: the EDID handed to IddCxMonitorCreate above
        // was generated from this very list (its preferred detailed timing
        // is monitor_modes[0]). Nothing is ever added to it — the target
        // list below is what UPDATE_MODES steers, always within it.
        let displaced = shell.monitors.lock().unwrap().insert(
            session_id,
            MonitorRt {
                monitor: OsHandle(monitor.cast()),
                edid,
                monitor_modes,
                target_modes,
                display_id,
                connector_index,
                adapter_luid,
                worker: None,
                ring,
                cursor: None,
            },
        );
        // A leaked prior entry (a plug that raced a final-exit drain):
        // treat the stale runtime as unplugged — stop its workers
        // (bounded) and mark its ring dead. Its monitor object died with
        // the old adapter, so no departure call.
        if let Some(mut prev) = displaced {
            if let Some(worker) = prev.worker.take() {
                worker.stop();
            }
            if let Some(cursor) = prev.cursor.as_mut() {
                cursor.stop();
            }
            prev.mark_ring_dead();
            forget_committed(prev.monitor);
        }

        let mut arrival: ffi::IDARG_OUT_MONITORARRIVAL = zeroed();
        let status = bindings::monitor_arrival(monitor, &mut arrival);
        tracelogging::write_event!(
            PROVIDER,
            "MonitorArrival",
            level(Informational),
            u64("session", &session_id),
            u32("connector", &connector_index),
            i32("status", &status)
        );
        if status != STATUS_SUCCESS {
            shell.monitors.lock().unwrap().remove(&session_id);
            return;
        }

        // Claim the hardware cursor plane (DESIGN.md §3.2.3). The worker
        // thread owns every cursor IddCx call — SetupHardwareCursor is
        // retried on its clock until the OS commits a path and accepts.
        // Nothing cursor-related ever runs inside an IddCx callback.
        let cursor = super::cursor::spawn(session_id, OsHandle(monitor.cast()));
        if let Some(rt) = shell.monitors.lock().unwrap().get_mut(&session_id) {
            rt.cursor = cursor;
        }
    }
}

/// Unplug: stop the frame + cursor workers (both bounded), mark the ring
/// DEAD so the host unmaps (bounded — a detached worker may still pin
/// the ring mutex, and this path runs on the effects worker, which must
/// never wedge), then IddCxMonitorDeparture. `rt` (and with it the
/// cursor event handle) drops only after departure returns, so the OS
/// never signals a closed event.
pub fn unplug(session_id: u64) {
    let shell = Shell::get();
    // A destroy landing while the session is parked in a TDR duck-out:
    // the monitor is already departed, so only the parked spec needs to
    // go (its ring is marked dead like the normal path below).
    {
        let mut ducked = shell.ducked.lock().unwrap();
        if let Some(pos) = ducked.iter().position(|d| d.session_id == session_id) {
            let d = ducked.remove(pos);
            drop(ducked);
            super::mark_ring_dead_arc(&d.ring);
            tracelogging::write_event!(
                PROVIDER,
                "UnplugWhileDucked",
                level(Informational),
                u64("session", &session_id)
            );
            return;
        }
    }
    let Some(mut rt) = shell.monitors.lock().unwrap().remove(&session_id) else {
        return;
    };
    if let Some(worker) = rt.worker.take() {
        worker.stop();
    }
    if let Some(cursor) = rt.cursor.as_mut() {
        cursor.stop();
    }
    rt.mark_ring_dead();
    forget_committed(rt.monitor);
    unsafe {
        let status = bindings::monitor_departure(rt.monitor.0.cast());
        tracelogging::write_event!(
            PROVIDER,
            "MonitorDeparture",
            level(Informational),
            u64("session", &session_id),
            i32("status", &status)
        );
    }
}

/// TDR duck-out, step 1: depart every live monitor so a failed OS TDR
/// recovery can never wait on the indirect display path, parking each
/// monitor's identity + ring for re-arrival. Runs on the effects worker
/// (departure is an IddCx call and re-enters the driver synchronously —
/// same constraints as unplug). Returns how many monitors were parked.
///
/// The parked ring is set REBUILDING, not DEAD: the host's capture layer
/// treats REBUILDING as "coming back" (same as a reassignment). (In
/// practice a real TDR outlasts the host's stale-heartbeat grace and it
/// reinitializes through the ordinary destroy/create paths anyway — the
/// mark is correctness, not the recovery contract.)
///
/// `expected_epoch` is the adapter epoch this duck was queued under: a
/// D3Final teardown can land mid-loop (its ducked drain and this loop's
/// pushes interleave), so on any epoch change the whole parked set is
/// self-drained here — otherwise entries pushed after the teardown's
/// drain would leak and later replug as ghost monitors.
pub fn duck_all(expected_epoch: u64) -> usize {
    duck_selected(expected_epoch, None)
}

/// Depart ONLY the named sessions (build 16's escalation arms).
///
/// `run_tdr_device_duck` scopes a device duck to the adapter LUID that
/// actually reported removal, precisely so a monitor rendering on a healthy
/// second GPU is not dragged into another adapter's reset — but the
/// escalation arms then called `duck_all` and departed everything anyway,
/// throwing that scoping away at the exact moment it mattered. The device
/// duck's arms use this instead; the legacy (build-14/15) duck keeps
/// `duck_all`, whose semantics are "every monitor, unconditionally".
pub fn duck_sessions(expected_epoch: u64, sessions: &[u64]) -> usize {
    duck_selected(expected_epoch, Some(sessions))
}

fn duck_selected(expected_epoch: u64, only: Option<&[u64]>) -> usize {
    let shell = super::Shell::get();
    let drained: Vec<(u64, super::MonitorRt)> = {
        let mut monitors = shell.monitors.lock().unwrap();
        match only {
            None => monitors.drain().collect(),
            Some(sessions) => sessions
                .iter()
                .filter_map(|sid| monitors.remove_entry(sid))
                .collect(),
        }
    };
    let mut parked = 0usize;
    for (session_id, mut rt) in drained {
        if let Some(worker) = rt.worker.take() {
            worker.stop();
        }
        if let Some(cursor) = rt.cursor.as_mut() {
            cursor.stop();
        }
        // Mirror unconditionally (one store, cannot fail), then a single
        // bounded attempt at the shared section: a detached worker may pin
        // the ring mutex, and unlike mark_ring_dead there is no urgency to
        // win — the host's stale-heartbeat detection covers an unmarked
        // ring, and the TDR poller reads the mirror.
        rt.ring
            .live
            .publish_state(luminal_driver_proto::ring_state::REBUILDING);
        if let Ok(ring) = rt.ring.ring.try_lock() {
            if let Some(s) = &ring.section {
                s.set_state(luminal_driver_proto::ring_state::REBUILDING);
            }
        }
        let status = unsafe { bindings::monitor_departure(rt.monitor.0.cast()) };
        tracelogging::write_event!(
            PROVIDER,
            "TdrDuckDeparted",
            level(Warning),
            u64("session", &session_id),
            i32("status", &status)
        );
        if status != STATUS_SUCCESS {
            // The monitor is still arrived — parking it would leave an
            // occupied connector that a replug (or the host's DESTROY,
            // whose ducked fast-path assumes already-departed) can never
            // reclaim. Put it back; unplug/teardown will depart it
            // through the normal path when its session ends.
            shell.monitors.lock().unwrap().insert(session_id, rt);
            continue;
        }
        forget_committed(rt.monitor);
        shell.ducked.lock().unwrap().push(super::DuckedMonitor {
            session_id,
            display_id: rt.display_id,
            connector_index: rt.connector_index,
            adapter_luid: rt.adapter_luid,
            edid: rt.edid,
            monitor_modes: rt.monitor_modes,
            target_modes: rt.target_modes,
            ring: rt.ring,
        });
        parked += 1;
    }
    // Teardown raced this loop: everything parked above may postdate the
    // D3Final drain. Clean up after ourselves — the epoch bump already
    // made the poller and any queued replug stale, so nothing else will.
    if shell.adapter_epoch() != expected_epoch {
        let stale: Vec<super::DuckedMonitor> = shell.ducked.lock().unwrap().drain(..).collect();
        for d in &stale {
            super::mark_ring_dead_arc(&d.ring);
        }
        tracelogging::write_event!(
            PROVIDER,
            "TdrDuckTornDownMidFlight",
            level(Warning),
            u64("drained", &(stale.len() as u64))
        );
        return 0;
    }
    parked
}

/// TDR duck-out, step 2: re-arrive every parked monitor after the display
/// stack recovered. Same container GUID (derived from display_id) and
/// connector, so Windows reattaches the remembered identity/topology.
/// Runs on the effects worker. A create/arrival failure drops that
/// monitor (traced, ring marked dead) — the host recreates the session
/// through the normal CREATE path on its next attempt.
pub fn replug_ducked() {
    let shell = super::Shell::get();
    let parked: Vec<super::DuckedMonitor> = {
        let mut ducked = shell.ducked.lock().unwrap();
        ducked.drain(..).collect()
    };
    let Some(adapter) = shell.adapter() else {
        // Adapter torn down while parked (device removal/re-add): the
        // fresh bring-up replugs from DeviceState instead.
        for d in &parked {
            super::mark_ring_dead_arc(&d.ring);
        }
        tracelogging::write_event!(
            PROVIDER,
            "TdrReplugNoAdapter",
            level(Warning),
            u64("count", &(parked.len() as u64))
        );
        return;
    };

    for d in parked {
        unsafe {
            let mut info: ffi::IDDCX_MONITOR_INFO = zeroed();
            info.Size = size_of::<ffi::IDDCX_MONITOR_INFO>() as u32;
            info.MonitorType = ffi::DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY_DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INDIRECT_WIRED;
            info.ConnectorIndex = d.connector_index;
            info.MonitorContainerId = container_guid(d.display_id);
            info.MonitorDescription.Size = size_of::<ffi::IDDCX_MONITOR_DESCRIPTION>() as u32;
            info.MonitorDescription.Type = ffi::IDDCX_MONITOR_DESCRIPTION_TYPE_IDDCX_MONITOR_DESCRIPTION_TYPE_EDID;
            info.MonitorDescription.DataSize = 256;
            info.MonitorDescription.pData = d.edid.as_ptr().cast::<core::ffi::c_void>().cast_mut();

            let mut in_args: ffi::IDARG_IN_MONITORCREATE = zeroed();
            in_args.pMonitorInfo = &mut info;
            let mut out_args: ffi::IDARG_OUT_MONITORCREATE = zeroed();
            let status = bindings::monitor_create(adapter.0.cast(), &in_args, &mut out_args);
            if status != STATUS_SUCCESS {
                tracelogging::write_event!(
                    PROVIDER,
                    "TdrReplugCreateFailed",
                    level(Error),
                    u64("session", &d.session_id),
                    i32("status", &status)
                );
                super::mark_ring_dead_arc(&d.ring);
                continue;
            }
            let monitor = out_args.MonitorObject;

            // Reinstate the runtime with the ORIGINAL ring Arc: sequences
            // and the generation continue, and the next assign retires
            // textures exactly like any reassignment.
            // The new monitor object is created from the SAME EDID, so it
            // gets the same monitor-mode superset; the published target
            // subset carries over too, or the re-arrival would silently
            // undo whatever gating was in force when the duck started.
            let displaced = shell.monitors.lock().unwrap().insert(
                d.session_id,
                super::MonitorRt {
                    monitor: super::OsHandle(monitor.cast()),
                    edid: d.edid,
                    monitor_modes: d.monitor_modes,
                    target_modes: d.target_modes,
                    display_id: d.display_id,
                    connector_index: d.connector_index,
                    adapter_luid: d.adapter_luid,
                    worker: None,
                    ring: d.ring,
                    cursor: None,
                },
            );
            if let Some(mut prev) = displaced {
                // A CREATE for the same session raced the replug (the
                // purge in plug() and this insert are not one atomic
                // step). Keep the newer entry's ring dead-marked and
                // stop its workers — mirror the displaced-entry handling
                // in plug().
                if let Some(worker) = prev.worker.take() {
                    worker.stop();
                }
                if let Some(cursor) = prev.cursor.as_mut() {
                    cursor.stop();
                }
                prev.mark_ring_dead();
                forget_committed(prev.monitor);
            }

            let mut arrival: ffi::IDARG_OUT_MONITORARRIVAL = zeroed();
            let status = bindings::monitor_arrival(monitor, &mut arrival);
            tracelogging::write_event!(
                PROVIDER,
                "TdrReplugged",
                level(Informational),
                u64("session", &d.session_id),
                i32("status", &status)
            );
            if status != STATUS_SUCCESS {
                if let Some(rt) = shell.monitors.lock().unwrap().remove(&d.session_id) {
                    rt.mark_ring_dead();
                    forget_committed(rt.monitor);
                }
                continue;
            }

            let cursor = super::cursor::spawn(d.session_id, super::OsHandle(monitor.cast()));
            if let Some(rt) = shell.monitors.lock().unwrap().get_mut(&d.session_id) {
                rt.cursor = cursor;
            }
        }
    }
}

/// A snapshot of one monitor's two mode lists, taken under the monitors
/// lock and handed to a DDI handler by value.
///
/// The split is the whole build-17 model: `monitor` is the frozen
/// description (reported by the parse DDIs), `targets` is the currently
/// published subset (reported by the query-target DDIs and pushed by
/// `IddCxMonitorUpdateModes2`). Windows offers the intersection, and
/// `targets ⊆ monitor` is an invariant every writer upholds, so the
/// intersection is exactly `targets` and is never empty.
struct ModeSnapshot {
    monitor: Vec<Mode>,
    targets: Vec<Mode>,
}

/// Every mode a LuminalVGD monitor object is created with comes out of the
/// EDID handed to `IddCxMonitorCreate` — the driver generates that EDID
/// from that list — so descriptor origin is the truthful answer for all of
/// them, and nothing is ever added afterwards to make it untrue.
/// (`..._ORIGIN_DRIVER` exists for modes the driver knows about separately
/// from the description. Build 17's first model needed it because it
/// appended to this list; the corrected model never does.)
const MONITOR_MODE_ORIGIN: ffi::IDDCX_MONITOR_MODE_ORIGIN =
    ffi::IDDCX_MONITOR_MODE_ORIGIN_IDDCX_MONITOR_MODE_ORIGIN_MONITORDESCRIPTOR;

// ---------------------------------------------------------------------
// UPDATE_MODES (build 17): replace the TARGET-mode list a LIVE monitor
// publishes, within the monitor-mode superset fixed at create.
// ---------------------------------------------------------------------

/// `IddCxMonitorUpdateModes2` calls in flight (0 or 1 in practice — only
/// the effects worker pushes). Read by `evt_d0_exit` through
/// [`drain_mode_pushes`], which is the D3Final half of the handshake that
/// keeps a teardown from destroying the adapter under a live push.
static PUSHES_IN_FLIGHT: AtomicU32 = AtomicU32::new(0);

/// How long a final device exit waits for an in-flight push to return.
/// Bounded like every other teardown wait (§3.3 rule 5); the wait sits
/// inside the multi-second worker drain D3Final already performs.
const PUSH_DRAIN_DEADLINE: Duration = Duration::from_millis(500);

/// Forget what a DEPARTED monitor had committed.
///
/// The record is replaced wholesale by the next commit, so this is not
/// what keeps it correct — but a departure is not always followed by a
/// commit, and an `IDDCX_MONITOR` handle can be reissued to a different
/// monitor object. A stale entry would then refuse that monitor's pushes
/// over a mode it never committed.
fn forget_committed(monitor: OsHandle) {
    COMMITTED.forget(monitor.0 as u64);
}

/// Forget every committed path: the adapter is going away and every
/// monitor object with it (`evt_d0_exit`).
pub(super) fn forget_all_committed() {
    COMMITTED.apply(&[], 0);
}

/// Publish a validated target subset on a live monitor.
///
/// Runs ONLY on the effects worker (via `Effect::UpdateModes`), because
/// `IddCxMonitorUpdateModes2` makes the OS re-enter our mode DDIs
/// synchronously on the calling thread.
///
/// # What is being changed, and what cannot be
///
/// `targets` is the TARGET-mode list to publish. The monitor's
/// DESCRIPTION — its monitor-mode set — is frozen at
/// `IddCxMonitorCreate`: `IDARG_IN_UPDATEMODES2` carries target modes
/// only, and no entry point in the IddCx 1.10/1.11 function table
/// replaces an arrived monitor's description. Windows offers the
/// intersection of the two lists (the intersection is skipped only for
/// remote drivers setting REMOTE_ALL_TARGET_MODES_MONITOR_COMPATIBLE,
/// which a console-session driver cannot set), so this call can gate and
/// steer within the superset and can never enlarge it. Enlarging is a
/// departure + recreate, which is the thing the opcode exists to avoid.
///
/// # The commit ordering (build 17 fix, kept)
///
/// `update_seq` names the update the session table is holding PENDING.
/// This function owes it exactly one `settle_modes`, and passes
/// `Applied` only when the OS really took the list. Nothing else in the
/// driver commits a published list. The original build-17 ordering
/// committed the durable list at IOCTL time, before the push — so a
/// refused or deferred push left durable state asserting a list the
/// monitor did not have, and the identical retry resolved to "nothing to
/// do", emitted no effect, and returned OK forever. Now a push that did
/// not take leaves every copy on the pre-update list and the retry pushes
/// again.
///
/// The settle carries the pushed LIST as well as the outcome, because
/// `update_seq` may already have been superseded by the time the DDI call
/// returns — the IOCTL takes the device lock only for its own dispatch and
/// this call holds no lock at all, so request #2's whole dispatch fits
/// inside push #1's call. A superseded push that the OS ACCEPTED cannot
/// read its own selection back out of `Monitor.pending_targets` (a newer
/// request replaced it) and must still record what the OS took, or the
/// durable list stays on the pre-update selection and the host's rescind
/// is short-circuited as "already published" for the life of the session.
/// See `SessionTable::settle_modes`.
///
/// # The lock protocol, which is the whole of the danger here
///
/// `evt_query_target_modes2`, `evt_parse_monitor_description2` and
/// `evt_assign` all take `shell.monitors`, and `std::sync::Mutex` is not
/// reentrant. Calling IddCx while holding that lock self-deadlocks the
/// effects worker the instant the OS re-enters — and a wedged effects
/// worker silently stalls every later plug, unplug, persist and TDR
/// effect. So: take the lock, publish the new list, copy out the handle,
/// build the target-mode array, DROP the guard, and only then call.
///
/// Publishing BEFORE the call (not after) is likewise deliberate: the
/// re-entrant query has to see the new list, or the OS is told about
/// modes the driver then denies it.
///
/// # Failure is always "target modes unchanged, carry on"
///
/// Constraint 1: an update the OS refuses degrades to the previously
/// published list. It never departs the monitor, never empties the target
/// list, never touches the ring, and never fails the session. The
/// rollback is best-effort by nature — the OS may already have consumed
/// the new list in a re-entrant query before returning failure — so the
/// trace records what happened rather than pretending the two views are
/// atomic. Both lists are non-empty subsets of the same frozen superset,
/// so whichever one the OS ends up holding is fully activatable.
pub fn update_modes(session_id: u64, update_seq: u64, targets: Vec<Mode>) {
    let shell = Shell::get();
    // `targets` is kept, not moved: it is the list this push puts in front
    // of the OS, and the settle below needs it. A push whose settle turns
    // out to be SUPERSEDED can no longer read its own selection out of
    // `Monitor.pending_targets` — a newer request replaced it — so the list
    // travels with the report instead.
    let push = push_targets(shell, session_id, &targets);

    // Constraint 6: every deny/fail path carries stage + code.
    match push {
        PushOutcome::AppliedParked { published, superset } => {
            // Deliberately NOT `UpdateModesApplied`: no OS call happened,
            // and that event's (modes, superset) pair is the
            // replace-vs-append measurement. This one says the parked spec
            // the re-arrival will plug with was patched — and, since build
            // 17's second pass, that it COMMITS, because the re-arrival is
            // that monitor's publication mechanism. A patch that did not
            // commit left the durable list and the parked spec disagreeing
            // forever, with the host's rescind short-circuited as "already
            // published".
            tracelogging::write_event!(
                PROVIDER,
                "UpdateModesParked",
                level(Informational),
                u64("session", &session_id),
                u32("stage", &stage::PARKED),
                u32("modes", &published),
                u32("superset", &superset),
                u32("applied", &1u32)
            );
        }
        PushOutcome::Applied { published, superset } => {
            // `published` vs `superset` is THE measurement this feature
            // turns on (see push_targets' replace-vs-append note): a
            // trace showing published < superset, then QueryTargetModes2
            // re-solicited with the same published count, then Display
            // Settings offering `published` modes, says the DDI replaces.
            // Display Settings still offering `superset` says it appends
            // (or that no re-solicit happened at all).
            tracelogging::write_event!(
                PROVIDER,
                "UpdateModesApplied",
                level(Informational),
                u64("session", &session_id),
                u32("modes", &published),
                u32("superset", &superset),
                i32("status", &STATUS_SUCCESS)
            );
        }
        PushOutcome::Blocked { committed, superset_idx, token, count } => {
            // The refusal, with everything needed to read it without
            // guessing: which mode the OS is running, where that mode sits
            // in this monitor's frozen description (the index the host is
            // handed on the wire), how many paths were active when it was
            // refused, and — the field that separates this event from every
            // other deny in the feature — that it is NOT worth retrying.
            // Before, this settled as an ordinary not-applied and a
            // retrying host looped on it forever.
            tracelogging::write_event!(
                PROVIDER,
                "UpdateModesGatesCommitted",
                level(Warning),
                u64("session", &session_id),
                u32("stage", &stage::GATES_COMMITTED),
                i32("code", &luminal_driver_proto::err::MODE_COMMITTED),
                u32("modes", &count),
                u32("committed_w", &committed.width),
                u32("committed_h", &committed.height),
                u32("committed_mhz", &committed.refresh_millihz),
                u32("blocking_mode", &superset_idx),
                u32("active_paths", &COMMITTED.active()),
                u64("commit_token", &token),
                u32("retryable", &u32::from(push.retryable()))
            );
        }
        PushOutcome::Deferred { stage, count } => {
            tracelogging::write_event!(
                PROVIDER,
                "UpdateModesDeferred",
                level(Informational),
                u64("session", &session_id),
                u32("stage", &stage),
                u32("modes", &count),
                // The half of a deferral that used to be missing: the host
                // is being told, through GET_STATUS, that the modes are NOT
                // in force and the request is worth sending again.
                u32("retryable", &u32::from(push.retryable()))
            );
        }
        PushOutcome::Failed { stage, code, count, rolled_back } => {
            tracelogging::write_event!(
                PROVIDER,
                "UpdateModesFailed",
                level(Warning),
                u64("session", &session_id),
                u32("stage", &stage),
                i32("code", &code),
                u32("modes", &count),
                u32("rolled_back", &u32::from(rolled_back)),
                // Every failure here IS retryable — the field exists so a
                // capture never has to infer that from the absence of the
                // committed-mode event.
                u32("retryable", &u32::from(push.retryable()))
            );
        }
    }

    // --- The durable commit, and the ONLY one. ---
    // Taken AFTER the IddCx call returned and with no other lock held —
    // the device lock is held for the whole of every dispatch(), so it
    // must never be on the far side of an OS call. `settle_modes` also
    // records the sticky per-monitor `err::UPDATE_FAILED` that carries a
    // not-applied outcome back to the host in every GET_STATUS reply,
    // which is the channel a retry decision is made on.
    //
    // `PushOutcome::settle_result` is the single mapping from outcome to
    // commit, and it lives in the portable layer with the tests that pin
    // it: anything that CHANGED a published list commits, anything that
    // changed nothing does not. Deciding that here, per arm, is how the
    // parked arm came to mutate a list it then settled `NotApplied`.
    //
    // `targets` rides along because a settle can be SUPERSEDED — a second
    // request's whole dispatch fits inside the DDI call above, since that
    // call is made with no lock held — and a superseded push that the OS
    // ACCEPTED still owes the table the list the OS took. Without it the
    // durable list stayed on the pre-update selection while the OS and the
    // runtime held this one, and the host's rescind was short-circuited as
    // "already published" for the life of the session.
    let result = push.settle_result();
    let outcome = shell
        .dev
        .lock()
        .unwrap()
        .table
        .settle_modes(session_id, update_seq, result, &targets);
    if !outcome.settled() {
        // The update was superseded by a newer one, or its session is
        // gone. Neither is an error — the newer update's own settle
        // decides the published list — but a silent miss and a missing
        // settle look identical in a capture, so say which. `committed`
        // separates the two superseded cases that behave differently: a
        // push the OS took wrote the durable list on its way past
        // (`SupersededCommitted`), one that changed nothing did not.
        tracelogging::write_event!(
            PROVIDER,
            "UpdateModesSettleStale",
            level(Informational),
            u64("session", &session_id),
            u64("seq", &update_seq),
            u32("committed", &u32::from(outcome.recorded()))
        );
    }
}

/// The push itself. Split out so the caller has exactly one settle site
/// and one trace site — a return path that forgot to settle would leave
/// the table holding a pending list forever, and the whole point of the
/// pending state is that it is always resolved.
///
/// # REPLACE vs APPEND — the undocumented part, and why it does not matter
///
/// Nothing in the IddCx 1.10 headers or on the
/// `IddCxMonitorUpdateModes2` reference page says whether the pushed list
/// REPLACES the monitor's target list or is APPENDED to it; the Remarks
/// say only "update the mode list previously reported for a monitor".
/// **This code assumes REPLACE** — that is what the request semantics are
/// written to express and what the ETW counts are worded for — but it is
/// correct either way, because of one invariant enforced right here: the
/// list handed to the OS is always a subset of the monitor-mode superset
/// this monitor object was created with (re-checked below against the
/// LIVE `monitor_modes`, not merely trusted from the session table).
/// Therefore:
///
/// - REPLACE  ⇒ the OS holds `targets`.
/// - APPEND   ⇒ the OS holds `previous ∪ targets`.
/// - RE-QUERY ⇒ the OS re-solicits `QueryTargetModes2`, which reports
///   `targets`, i.e. replace.
///
/// In all three the OS's target list is a NON-EMPTY subset of the frozen
/// superset, so the monitor∩target intersection is non-empty and every
/// mode in it is one the monitor really describes: no unactivatable mode,
/// no monitor left with no targets, no failed session. The only
/// difference is effectiveness — under APPEND a gate does not actually
/// remove the old rate, it just adds nothing, and the update is a no-op
/// the host can detect.
///
/// **How a reader tells which one it is** (the signing round settles it):
/// push a strict subset (published < superset), then in the trace read
/// `UpdateModesApplied(modes, superset)`, whether `QueryTargetModes2`
/// reappears with `published` equal to the pushed count, and finally
/// count the rates Display Settings offers. `published` ⇒ replace or
/// re-query; `superset` ⇒ append (or no re-solicit). Do not assert either
/// in code or docs until that is measured.
fn push_targets(shell: &Shell, session_id: u64, targets: &[Mode]) -> PushOutcome {
    // `IDARG_IN_UPDATEMODES2.TargetModeCount` "cannot be zero"
    // (IddCx.h:3594). The core layer refuses an empty selection before it
    // can ever become an effect, so this is the second gate, not the
    // first — but it is the one standing directly in front of the OS
    // call, and it must never be removed.
    if targets.is_empty() {
        return PushOutcome::Failed {
            stage: stage::EMPTY,
            code: luminal_driver_proto::err::BAD_MODE,
            count: 0,
            rolled_back: false,
        };
    }
    let count = targets.len() as u32;

    // A session parked by a TDR duck-out has no monitor object to push
    // at — it was departed, and the replug creates a NEW one. Patch the
    // parked selection so the re-arrival publishes it; without this the
    // update would be silently undone by the recovery.
    //
    // `modepush::parked_push` decides, and `commits()` is what says the
    // patch may be written at all — the two are one decision on purpose.
    // Build 17 wrote the patch and then settled `NotApplied`, so the
    // parked spec (and the monitor the replug re-arrived) published the
    // new subset while the durable list still held the old one; the
    // host's rescind then matched the stale durable list, was answered
    // "already published", emitted no effect, and left the monitor gated
    // for the life of the session. A patch changes what the monitor
    // publishes, so it commits.
    {
        let mut ducked = shell.ducked.lock().unwrap();
        if let Some(d) = ducked.iter_mut().find(|d| d.session_id == session_id) {
            let outcome = modepush::parked_push(&d.monitor_modes, targets);
            if outcome.commits() {
                d.target_modes = targets.to_vec();
            }
            return outcome;
        }
    }

    // Deferral checks come BEFORE anything is published. Both are
    // lock-free reads of state the effects worker cannot race with
    // itself, and taking them here means a deferral leaves the runtime
    // list exactly as it was — the build-17 original published first and
    // returned, leaving the runtime list ahead of a durable list it never
    // committed, which is the divergence in a second guise.
    //
    // A duck in flight: the monitor is arrived but its transport is
    // REBUILDING and the recovery poller is judging whether the OS
    // re-assigns on its own. Do not hand that decision a mode-list change
    // mid-flight; the host retries once the duck settles.
    if shell.tdr_duck_pending.load(Ordering::SeqCst) {
        return PushOutcome::Deferred { stage: stage::DUCK_PENDING, count };
    }

    // --- The D3Final handshake (see `drain_mode_pushes`). ---
    // A final device exit clears the adapter and the OS destroys its child
    // monitor objects with it, so a push must never straddle one. The
    // in-flight mark is taken FIRST and the adapter read after it, while
    // `evt_d0_exit` clears the adapter first and reads the mark after:
    // with a SeqCst fence on both sides at least one of the two sees the
    // other, so either this push observes the cleared adapter and never
    // calls, or the teardown observes this push and waits for it.
    // Checking the adapter alone — which is all build 17 did — left the
    // whole interval between the check and the DDI call unguarded.
    let Some((_in_flight, epoch)) = PushInFlight::begin(shell) else {
        return PushOutcome::Deferred { stage: stage::NO_ADAPTER, count };
    };

    // --- Under the lock: validate, publish, snapshot, build. No IddCx. ---
    let (monitor, previous, superset, target_modes) = {
        let mut monitors = shell.monitors.lock().unwrap();
        let Some(rt) = monitors.get_mut(&session_id) else {
            return PushOutcome::Failed {
                stage: stage::NO_MONITOR,
                code: luminal_driver_proto::err::NO_SUCH_SESSION,
                count,
                rolled_back: false,
            };
        };
        let monitor = rt.monitor;
        // The two checks that stand directly in front of the OS call, both
        // against the LIVE monitor object rather than the session table's
        // copy of it: `targets ⊆ superset` (the invariant that keeps this
        // safe under both replace and append semantics), and "does this
        // gate out the mode the OS has COMMITTED". See `modepush::live_gate`
        // for why the second one refuses rather than proceeds.
        //
        // The token is read WITH the committed mode and travels with the
        // refusal: it is what tells a later request whether that refusal is
        // still evidence of anything (`SessionTable::update_modes`).
        let (committed, commit_token) = COMMITTED.get_with_token(monitor.0 as u64);
        match modepush::live_gate(&rt.monitor_modes, targets, committed) {
            LiveGate::Push => {}
            LiveGate::PushCommittedUnrecognised(committed) => {
                // FAIL-OPEN, and this is the event that says so. The OS
                // committed a mode this monitor's frozen description does
                // not contain, so the committed-mode rule cannot be applied
                // to it and the push proceeds exactly as it did before the
                // gate existed. Untraced — as it was when the gate landed —
                // it is indistinguishable from a monitor with nothing
                // committed, which is the difference between "the feature
                // is working" and "the feature has quietly stopped
                // deciding anything", e.g. after a signal-decoding change.
                tracelogging::write_event!(
                    PROVIDER,
                    "UpdateModesCommittedUnrecognised",
                    level(Warning),
                    u64("session", &session_id),
                    u32("stage", &stage::GATES_COMMITTED),
                    u32("modes", &count),
                    u32("superset", &(rt.monitor_modes.len() as u32)),
                    u32("committed_w", &committed.width),
                    u32("committed_h", &committed.height),
                    u32("committed_mhz", &committed.refresh_millihz),
                    u32("active_paths", &COMMITTED.active())
                );
            }
            LiveGate::NotInSuperset => {
                return PushOutcome::Failed {
                    stage: stage::NOT_IN_SUPERSET,
                    code: luminal_driver_proto::err::BAD_MODE,
                    count,
                    rolled_back: false,
                };
            }
            LiveGate::EvictsCommitted { committed, superset_idx } => {
                // The one scenario this feature must not cause: under
                // `virtual_display_layout=exclusive` this is the only
                // active display, and publishing a subset without its
                // committed mode makes Windows re-select — a modeset in
                // the middle of the stream. Refuse the PUSH, never the
                // session: the previously published list stays in force and
                // the monitor and the stream carry on.
                //
                // Reported as `Blocked`, not `Failed`: it is the one
                // refusal a retry cannot fix, and the host is told so
                // (`err::MODE_COMMITTED` + `update_status::BLOCKED`) along
                // with `superset_idx` — which mode is in the way — so it
                // can publish a subset that keeps it, or do the modeset
                // itself first. The trace is emitted by the single trace
                // site in `update_modes`, from this outcome.
                return PushOutcome::Blocked {
                    committed,
                    superset_idx,
                    token: commit_token,
                    count,
                };
            }
        }
        let superset = rt.monitor_modes.len() as u32;
        let previous = core::mem::replace(&mut rt.target_modes, targets.to_vec());
        // monitor_modes is deliberately untouched: the EDID that
        // describes it cannot be reissued, and nothing here claims to
        // change what the monitor IS — only what it currently offers.
        let mut target_modes: Vec<ffi::IDDCX_TARGET_MODE2> =
            vec![unsafe { zeroed() }; rt.target_modes.len()];
        for (slot, mode) in target_modes.iter_mut().zip(rt.target_modes.iter()) {
            fill_target_mode2(slot, mode);
        }
        (monitor, previous, superset, target_modes)
    };

    // The list is published but the OS has not been told. A teardown that
    // landed while the lock was held (it takes the monitors lock only for
    // its drain, which is AFTER clear_adapter) means the monitor object is
    // being destroyed: put the runtime list back and defer, rather than
    // calling into a dying adapter.
    if shell.adapter_epoch() != epoch {
        let (rolled_back, _) = restore_targets(shell, session_id, monitor, targets, previous);
        tracelogging::write_event!(
            PROVIDER,
            "UpdateModesAdapterTornDown",
            level(Warning),
            u64("session", &session_id),
            u32("stage", &stage::ADAPTER_TORN_DOWN),
            u32("modes", &count),
            u32("rolled_back", &u32::from(rolled_back))
        );
        return PushOutcome::Deferred { stage: stage::ADAPTER_TORN_DOWN, count };
    }

    // --- No locks held. The OS may re-enter our DDIs inside this call. ---
    let status = unsafe {
        let mut in_args: ffi::IDARG_IN_UPDATEMODES2 = zeroed();
        // CONFIGURATION_CONSTRAINTS: the set of modes that is valid right
        // now changed because of how the display is configured (the host
        // gating on the client's current workload). Not POWER_CONSTRAINTS
        // and not either bandwidth reason — those name specific physical
        // limits we are not reporting.
        in_args.Reason =
            ffi::IDDCX_UPDATE_REASON_IDDCX_UPDATE_REASON_CONFIGURATION_CONSTRAINTS;
        in_args.TargetModeCount = count;
        in_args.pTargetModes = target_modes.as_ptr().cast_mut();
        bindings::monitor_update_modes2(monitor.0.cast(), &in_args)
    };
    // `target_modes` stays alive until here by construction — the OS only
    // borrows it for the duration of the call.

    if status == STATUS_SUCCESS {
        return PushOutcome::Applied { published: count, superset };
    }

    // --- Failure: restore the previously published list. ---
    let (rolled_back, raced) = restore_targets(shell, session_id, monitor, targets, previous);
    PushOutcome::Failed {
        stage: if raced { stage::ROLLBACK_RACED } else { stage::OS_CALL },
        code: status,
        count,
        rolled_back,
    }
}

/// Put `previous` back as the monitor's published target list, but only if
/// this call's own write is still there to undo. Returns
/// `(rolled_back, raced)`.
///
/// Re-verifies identity first (the AssignRacedUnplug pattern): between
/// dropping the guard and here the session can have been destroyed,
/// reaped, ducked, or replugged onto a NEW monitor object, and writing a
/// stale list onto a fresh monitor would be worse than the failure being
/// rolled back. The list comparison is the second half of that check —
/// the restore runs only when the runtime list is still, entry for entry,
/// the one this call published, so it can undo its own write and never a
/// later update's. What it puts back was itself a non-empty subset of the
/// same frozen superset, so a rollback can produce neither an empty nor an
/// unactivatable target list.
fn restore_targets(
    shell: &Shell,
    session_id: u64,
    monitor: OsHandle,
    pushed: &[Mode],
    previous: Vec<Mode>,
) -> (bool, bool) {
    let mut monitors = shell.monitors.lock().unwrap();
    match monitors.get_mut(&session_id) {
        Some(rt) if rt.monitor == monitor && rt.target_modes == pushed => {
            rt.target_modes = previous;
            (true, false)
        }
        _ => (false, true),
    }
}

/// RAII mark for "an `IddCxMonitorUpdateModes2` is in flight", the push
/// half of the D3Final handshake in [`drain_mode_pushes`].
struct PushInFlight;

impl PushInFlight {
    /// Mark, then read the adapter. `None` when the adapter is already
    /// gone (the mark is dropped again first).
    ///
    /// The ORDER is the guarantee. This stores, fences, then loads the
    /// adapter; `evt_d0_exit` clears the adapter, fences, then loads the
    /// mark. Two SeqCst fences between a store and a load on each side is
    /// the standard mutual-exclusion argument: it is impossible both for
    /// this to read a live adapter and for the teardown to read no push.
    fn begin(shell: &Shell) -> Option<(Self, u64)> {
        PUSHES_IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        // Handle and epoch together under one lock: read apart, a clear
        // landing between them yields a live handle with the NEW epoch,
        // and the epoch re-check before the OS call would then pass.
        match shell.adapter_with_epoch() {
            Some((_, epoch)) => Some((Self, epoch)),
            None => {
                PUSHES_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
                None
            }
        }
    }
}

impl Drop for PushInFlight {
    fn drop(&mut self) {
        PUSHES_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Wait (bounded) for any in-flight `IddCxMonitorUpdateModes2` to return.
///
/// Called from `evt_d0_exit` AFTER `clear_adapter`, which is what makes
/// the handshake work: a push that starts from here on sees no adapter and
/// defers, and a push already running is waited for instead of having its
/// monitor object destroyed underneath it. `push_targets` also re-checks
/// the adapter epoch after publishing and before calling, so the teardown
/// never has to win the race — only to not lose it silently.
///
/// Bounded like every other teardown wait (§3.3 rule 5): on deadline the
/// exit proceeds and the trace says so. That is the pre-existing exposure,
/// not a new one.
pub(super) fn drain_mode_pushes() {
    fence(Ordering::SeqCst);
    let deadline = Instant::now() + PUSH_DRAIN_DEADLINE;
    loop {
        if PUSHES_IN_FLIGHT.load(Ordering::SeqCst) == 0 {
            return;
        }
        if Instant::now() >= deadline {
            tracelogging::write_event!(PROVIDER, "UpdateModesDrainTimeout", level(Warning));
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Find the session whose EDID identity octets match `desc` bytes 8..16.
fn session_modes_for_edid(data: &[u8]) -> Option<ModeSnapshot> {
    if data.len() < 128 {
        return None;
    }
    let shell = Shell::get();
    let monitors = shell.monitors.lock().unwrap();
    monitors
        .values()
        .find(|rt| rt.edid[8..16] == data[8..16])
        .map(|rt| ModeSnapshot {
            monitor: rt.monitor_modes.clone(),
            targets: rt.target_modes.clone(),
        })
}

fn modes_for_monitor_object(monitor: ffi::IDDCX_MONITOR) -> Option<ModeSnapshot> {
    let shell = Shell::get();
    let monitors = shell.monitors.lock().unwrap();
    monitors
        .values()
        .find(|rt| rt.monitor == OsHandle(monitor.cast()))
        .map(|rt| ModeSnapshot {
            monitor: rt.monitor_modes.clone(),
            targets: rt.target_modes.clone(),
        })
}

/// Fill one `IDDCX_TARGET_MODE2` slot from a mode.
///
/// THE single definition, shared by `evt_query_target_modes2` (the OS
/// asking) and `update_modes` (the driver pushing). Build 17 added the
/// second caller; keeping one function is what makes it impossible for
/// the pushed list and the queried list to describe the same mode
/// differently — a divergence the OS would resolve by simply not
/// activating the mode, with nothing anywhere saying why.
fn fill_target_mode2(slot: &mut ffi::IDDCX_TARGET_MODE2, mode: &Mode) {
    slot.Size = size_of::<ffi::IDDCX_TARGET_MODE2>() as u32;
    slot.TargetVideoSignalInfo.targetVideoSignalInfo = signal_info(mode, 1);
    // Zero, matching MaxDisplayPipelineRate = 0: bandwidth management
    // unused. A nonzero requirement against a zero adapter budget makes
    // every mode unactivatable (Extend reverts, Scale/Res grayed).
    slot.RequiredBandwidth = 0;
    slot.BitsPerComponent = wire_bpc_for(mode);
}

/// Build the DISPLAYCONFIG signal block for one mode. Zero-blanking
/// timings (total == active), the IddSampleDriver convention for virtual
/// displays. `divider` is 0 for monitor modes and ≥1 for target modes,
/// per the IddCx.h contract.
fn signal_info(mode: &Mode, divider: u32) -> ffi::DISPLAYCONFIG_VIDEO_SIGNAL_INFO {
    const D3DKMDT_VSS_OTHER: u32 = 255;
    let mut sig: ffi::DISPLAYCONFIG_VIDEO_SIGNAL_INFO = unsafe { zeroed() };
    sig.pixelRate =
        (mode.width as u64) * (mode.height as u64) * (mode.refresh_millihz as u64) / 1000;
    sig.hSyncFreq.Numerator = mode.refresh_millihz.saturating_mul(mode.height) / 1000;
    sig.hSyncFreq.Denominator = 1;
    sig.vSyncFreq.Numerator = mode.refresh_millihz;
    sig.vSyncFreq.Denominator = 1000;
    sig.activeSize.cx = mode.width;
    sig.activeSize.cy = mode.height;
    sig.totalSize = sig.activeSize;
    unsafe {
        sig.__bindgen_anon_1
            .AdditionalSignalInfo
            .set_videoStandard(D3DKMDT_VSS_OTHER);
        sig.__bindgen_anon_1
            .AdditionalSignalInfo
            .set_vSyncFreqDivider(divider);
    }
    sig.scanLineOrdering = ffi::DISPLAYCONFIG_SCANLINE_ORDERING_DISPLAYCONFIG_SCANLINE_ORDERING_PROGRESSIVE;
    sig
}

pub unsafe extern "C" fn evt_parse_monitor_description(
    in_args: *const ffi::IDARG_IN_PARSEMONITORDESCRIPTION,
    out_args: *mut ffi::IDARG_OUT_PARSEMONITORDESCRIPTION,
) -> NTSTATUS {
    let inp = &*in_args;
    let out = &mut *out_args;
    let desc = &inp.MonitorDescription;
    if desc.pData.is_null() || desc.DataSize < 128 {
        return STATUS_INVALID_PARAMETER;
    }
    let data = core::slice::from_raw_parts(desc.pData.cast::<u8>(), desc.DataSize as usize);
    let Some(snapshot) = session_modes_for_edid(data) else {
        return STATUS_INVALID_PARAMETER;
    };

    // The MONITOR list: the frozen superset, never the published subset.
    // `UPDATE_MODES` cannot change what is reported here, which is what
    // keeps this list constant for the life of the monitor. The PREFERRED
    // INDEX is not constant, though — see `evt_parse_monitor_description2`.
    out.MonitorModeBufferOutputCount = snapshot.monitor.len() as u32;
    out.PreferredMonitorModeIdx =
        modepush::preferred_monitor_mode_idx(&snapshot.monitor, &snapshot.targets, usize::MAX);
    if inp.MonitorModeBufferInputCount == 0 || inp.pMonitorModes.is_null() {
        return STATUS_SUCCESS;
    }
    let fill = snapshot.monitor.len().min(inp.MonitorModeBufferInputCount as usize);
    let slots = core::slice::from_raw_parts_mut(inp.pMonitorModes, fill);
    for (slot, mode) in slots.iter_mut().zip(snapshot.monitor.iter()) {
        slot.Size = size_of::<ffi::IDDCX_MONITOR_MODE>() as u32;
        slot.Origin = MONITOR_MODE_ORIGIN;
        slot.MonitorVideoSignalInfo = signal_info(mode, 0);
    }
    out.MonitorModeBufferOutputCount = fill as u32;
    out.PreferredMonitorModeIdx =
        modepush::preferred_monitor_mode_idx(&snapshot.monitor, &snapshot.targets, fill);
    STATUS_SUCCESS
}

pub unsafe extern "C" fn evt_default_modes(
    _monitor: ffi::IDDCX_MONITOR,
    _in_args: *const ffi::IDARG_IN_GETDEFAULTDESCRIPTIONMODES,
    out_args: *mut ffi::IDARG_OUT_GETDEFAULTDESCRIPTIONMODES,
) -> NTSTATUS {
    // Every LuminalVGD monitor carries an EDID, so the description-less
    // path never produces modes.
    let out = &mut *out_args;
    out.DefaultMonitorModeBufferOutputCount = 0;
    out.PreferredMonitorModeIdx = ffi::NO_PREFERRED_MODE;
    STATUS_SUCCESS
}

pub unsafe extern "C" fn evt_query_target_modes(
    monitor: ffi::IDDCX_MONITOR,
    in_args: *const ffi::IDARG_IN_QUERYTARGETMODES,
    out_args: *mut ffi::IDARG_OUT_QUERYTARGETMODES,
) -> NTSTATUS {
    let inp = &*in_args;
    let out = &mut *out_args;
    let Some(snapshot) = modes_for_monitor_object(monitor) else {
        return STATUS_INVALID_PARAMETER;
    };

    // The TARGET list: the currently published subset, which is what
    // `UPDATE_MODES` steers and what the OS intersects with the monitor
    // description.
    out.TargetModeBufferOutputCount = snapshot.targets.len() as u32;
    if inp.TargetModeBufferInputCount == 0 || inp.pTargetModes.is_null() {
        return STATUS_SUCCESS;
    }
    let fill = snapshot.targets.len().min(inp.TargetModeBufferInputCount as usize);
    let slots = core::slice::from_raw_parts_mut(inp.pTargetModes, fill);
    for (slot, mode) in slots.iter_mut().zip(snapshot.targets.iter()) {
        slot.Size = size_of::<ffi::IDDCX_TARGET_MODE>() as u32;
        slot.TargetVideoSignalInfo.targetVideoSignalInfo = signal_info(mode, 1);
        // Zero, matching MaxDisplayPipelineRate = 0: bandwidth management
        // unused. A nonzero requirement against a zero adapter budget makes
        // every mode unactivatable (Extend reverts, Scale/Res grayed).
        slot.RequiredBandwidth = 0;
    }
    out.TargetModeBufferOutputCount = fill as u32;
    STATUS_SUCCESS
}

pub unsafe extern "C" fn evt_commit_modes(
    _adapter: ffi::IDDCX_ADAPTER,
    _in_args: *const ffi::IDARG_IN_COMMITMODES,
) -> NTSTATUS {
    // Mode state lives OS-side; nothing to reconcile until the frame
    // pipeline (phase 4) cares about the committed mode.
    STATUS_SUCCESS
}

// ---------------------------------------------------------------------
// IddCx ≥1.4 mandatory DDIs. Declaring client version 1.10 makes the OS
// validate these at device start (missing ⇒ STATUS_DEVICE_CONFIGURATION_
// ERROR). Phase-2 scope is SDR: 8-bit RGB wire format, no HDR caps; the
// HDR paths get real implementations alongside caps::HDR10 later.
// ---------------------------------------------------------------------

/// Wire format for one mode: RGB at the session's bit depth (8 for SDR-8,
/// 10 for SDR-10/HDR10, 12 for HDR12). The OS picks the highest depth the
/// whole path supports; HDR additionally needs the EDID's CTA-861.3 block
/// (core::edid) and the adapter's CAN_PROCESS_FP16 flag (shell::entry).
fn wire_bpc_for(mode: &Mode) -> ffi::IDDCX_WIRE_BITS_PER_COMPONENT {
    use luminal_driver_proto::BitDepth;
    let mut bpc: ffi::IDDCX_WIRE_BITS_PER_COMPONENT = unsafe { zeroed() };
    bpc.Rgb = match mode.bit_depth {
        BitDepth::Sdr8 => ffi::IDDCX_BITS_PER_COMPONENT_IDDCX_BITS_PER_COMPONENT_8,
        BitDepth::Sdr10 | BitDepth::Hdr10 => {
            ffi::IDDCX_BITS_PER_COMPONENT_IDDCX_BITS_PER_COMPONENT_10
        }
        BitDepth::Hdr12 => ffi::IDDCX_BITS_PER_COMPONENT_IDDCX_BITS_PER_COMPONENT_12,
    };
    bpc
}

pub unsafe extern "C" fn evt_adapter_query_target_info(
    _adapter: ffi::IDDCX_ADAPTER,
    _in_args: *mut ffi::IDARG_IN_QUERYTARGET_INFO,
    out_args: *mut ffi::IDARG_OUT_QUERYTARGET_INFO,
) -> NTSTATUS {
    let out = &mut *out_args;
    // HDR10/advanced color needs the target to advertise the wide/high
    // color-space pipeline; harmless for SDR-only monitors (the OS still
    // gates on the per-monitor EDID + wire bit depth).
    out.TargetCaps = ffi::IDDCX_TARGET_CAPS_IDDCX_TARGET_CAPS_HIGH_COLOR_SPACE
        | ffi::IDDCX_TARGET_CAPS_IDDCX_TARGET_CAPS_WIDE_COLOR_SPACE;
    out.DitheringSupport = zeroed();
    STATUS_SUCCESS
}

pub unsafe extern "C" fn evt_parse_monitor_description2(
    in_args: *const ffi::IDARG_IN_PARSEMONITORDESCRIPTION2,
    out_args: *mut ffi::IDARG_OUT_PARSEMONITORDESCRIPTION,
) -> NTSTATUS {
    let inp = &*in_args;
    let out = &mut *out_args;
    let desc = &inp.MonitorDescription;
    if desc.pData.is_null() || desc.DataSize < 128 {
        tracelogging::write_event!(
            PROVIDER,
            "ParseDescription2Bad",
            level(Warning),
            u32("size", &desc.DataSize)
        );
        return STATUS_INVALID_PARAMETER;
    }
    let data = core::slice::from_raw_parts(desc.pData.cast::<u8>(), desc.DataSize as usize);
    let Some(snapshot) = session_modes_for_edid(data) else {
        // The OS is asking about an EDID no live session owns — activation
        // cannot proceed for that monitor. Cold-boot instrumentation
        // (2026-07-25): the build-12 activation failure needed exactly
        // this visibility.
        tracelogging::write_event!(
            PROVIDER,
            "ParseDescription2NoSession",
            level(Warning)
        );
        return STATUS_INVALID_PARAMETER;
    };
    // THE PREFERRED INDEX MOVES WITH THE GATE. It was a constant 0 — the
    // EDID's preferred detailed timing, `monitor_modes[0]` — which stopped
    // being right the moment `UPDATE_MODES` could publish a target subset
    // that excludes entry 0: Windows offers the INTERSECTION of the two
    // lists, so a preferred index pointing outside the published subset
    // names a mode the OS cannot activate, and nothing in a trace says
    // why. Report the first monitor mode that is actually being offered.
    // (`targets ⊆ superset` makes a match certain; the helper falls back
    // to 0 rather than ever returning an out-of-range index.)
    //
    // Computed ONCE, before the trace, because the index depends on how
    // much of the list is being reported: a count-only query names an entry
    // from the whole list, while a filled query may only name one inside
    // the buffer the OS gave us. The first cut traced the whole-list answer
    // and then reported the truncated one — so the field a reader would use
    // to explain the OS's default choice was, in exactly the truncated case
    // worth investigating, not the value the OS was given.
    let count = snapshot.monitor.len();
    let fill = if inp.MonitorModeBufferInputCount == 0 || inp.pMonitorModes.is_null() {
        None
    } else {
        Some(count.min(inp.MonitorModeBufferInputCount as usize))
    };
    let preferred = modepush::preferred_monitor_mode_idx(
        &snapshot.monitor,
        &snapshot.targets,
        fill.unwrap_or(usize::MAX),
    );

    // `modes` here is the SUPERSET and is expected to be constant for the
    // life of the monitor — an `UPDATE_MODES` can never move it, so a
    // trace where it changes means something re-created the monitor.
    // `published` rides along so one line of a capture shows the gate
    // this monitor is currently under: published < modes means a subset
    // is being offered. `filled` is how many modes this call actually
    // wrote, which is what `preferred` indexes into.
    tracelogging::write_event!(
        PROVIDER,
        "ParseDescription2",
        level(Informational),
        u32("modes", &(count as u32)),
        u32("published", &(snapshot.targets.len() as u32)),
        u32("buffer", &inp.MonitorModeBufferInputCount),
        u32("filled", &(fill.unwrap_or(0) as u32)),
        // Moves with the gate since build 17's second pass — a trace where
        // `preferred` is nonzero says entry 0 is currently gated out. This
        // is the value REPORTED, not a second computation of it.
        u32("preferred", &preferred)
    );

    out.MonitorModeBufferOutputCount = count as u32;
    out.PreferredMonitorModeIdx = preferred;
    let Some(fill) = fill else {
        return STATUS_SUCCESS;
    };
    let slots = core::slice::from_raw_parts_mut(inp.pMonitorModes, fill);
    for (slot, mode) in slots.iter_mut().zip(snapshot.monitor.iter()) {
        slot.Size = size_of::<ffi::IDDCX_MONITOR_MODE2>() as u32;
        slot.Origin = MONITOR_MODE_ORIGIN;
        slot.MonitorVideoSignalInfo = signal_info(mode, 0);
        slot.BitsPerComponent = wire_bpc_for(mode);
    }
    out.MonitorModeBufferOutputCount = fill as u32;
    STATUS_SUCCESS
}

pub unsafe extern "C" fn evt_query_target_modes2(
    monitor: ffi::IDDCX_MONITOR,
    in_args: *const ffi::IDARG_IN_QUERYTARGETMODES2,
    out_args: *mut ffi::IDARG_OUT_QUERYTARGETMODES,
) -> NTSTATUS {
    let inp = &*in_args;
    let out = &mut *out_args;
    let Some(snapshot) = modes_for_monitor_object(monitor) else {
        tracelogging::write_event!(
            PROVIDER,
            "QueryTargetModes2NoSession",
            level(Warning),
            u64("monitor_ptr", &(monitor as u64))
        );
        return STATUS_INVALID_PARAMETER;
    };
    // `modes` is the published TARGET count and `superset` the monitor
    // description's size. This pairing is the build-17 measurement: after
    // an `UpdateModesApplied(modes=N, superset=M)` with N < M, seeing
    // this event again with modes=N is the OS re-soliciting the target
    // list (so a replace really took), and NOT seeing it says the push
    // was absorbed without a re-query — at which point what Display
    // Settings offers decides replace vs append.
    tracelogging::write_event!(
        PROVIDER,
        "QueryTargetModes2",
        level(Informational),
        u32("modes", &(snapshot.targets.len() as u32)),
        u32("superset", &(snapshot.monitor.len() as u32)),
        u32("buffer", &inp.TargetModeBufferInputCount)
    );

    out.TargetModeBufferOutputCount = snapshot.targets.len() as u32;
    if inp.TargetModeBufferInputCount == 0 || inp.pTargetModes.is_null() {
        return STATUS_SUCCESS;
    }
    let fill = snapshot.targets.len().min(inp.TargetModeBufferInputCount as usize);
    let slots = core::slice::from_raw_parts_mut(inp.pTargetModes, fill);
    for (slot, mode) in slots.iter_mut().zip(snapshot.targets.iter()) {
        fill_target_mode2(slot, mode);
    }
    out.TargetModeBufferOutputCount = fill as u32;
    STATUS_SUCCESS
}

/// The OS committing a path set on our adapter — and, since build 17, the
/// only place the driver can learn WHICH MODE the desktop is actually
/// running on each of our monitors.
///
/// # Why the path list is captured
///
/// `IddCxMonitorUpdateModes2` publishes a target subset. If that subset
/// excludes the mode the OS has committed, Windows must re-select — a
/// modeset on what is, under `virtual_display_layout=exclusive`, the ONLY
/// active display, in the middle of the stream the whole feature exists to
/// avoid disturbing. Build 17 discarded `IDDCX_PATH2` entirely, so the
/// driver could neither refuse such a push nor even say in a trace that it
/// had made one. `push_targets` now consults [`COMMITTED`] immediately
/// before the call (`modepush::live_gate`).
///
/// # Callback-frame discipline (constraint 4)
///
/// This is a win32k-side modeset transaction on our thread. It therefore
/// takes NO lock, allocates nothing, and calls nothing that can block: a
/// fixed-size stack array is filled from the path list and published into
/// a lock-free record with a bounded number of atomic stores. It must stay
/// that way — in particular it must never take `shell.monitors`, which
/// `push_targets` holds while building its target array.
pub unsafe extern "C" fn evt_commit_modes2(
    _adapter: ffi::IDDCX_ADAPTER,
    in_args: *const ffi::IDARG_IN_COMMITMODES2,
) -> NTSTATUS {
    let inp = &*in_args;
    let count = inp.PathCount;
    let mut recorded: [(u64, CommittedMode); MAX_COMMITTED_PATHS] =
        [(0, CommittedMode { width: 0, height: 0, refresh_millihz: 0 }); MAX_COMMITTED_PATHS];
    let mut active = 0usize;
    let mut skipped = 0u32;
    // ACTIVE paths in this commit, counted before the array runs out.
    // `CommittedPaths::active()` is what `UpdateModesGatesCommitted`
    // reports, and "how many displays were active when this push was
    // refused" must not be silently capped at the slot count: deriving it
    // from the recorded entries (as the first cut did) made the documented
    // "may exceed the slots recorded" unreachable and would understate
    // exactly the configuration worth looking at.
    let mut active_total = 0u32;
    if !inp.pPaths.is_null() {
        let paths = core::slice::from_raw_parts(inp.pPaths, count as usize);
        for path in paths {
            let is_active = path.Flags
                & ffi::IDDCX_PATH_FLAGS_IDDCX_PATH_FLAGS_ACTIVE
                != 0;
            let sig = &path.TargetVideoSignalInfo;
            let mode = CommittedMode::from_signal(
                sig.activeSize.cx,
                sig.activeSize.cy,
                u64::from(sig.vSyncFreq.Numerator),
                u64::from(sig.vSyncFreq.Denominator),
            );
            // Trace EVERY path, active or not, with what it committed:
            // "which mode is this display running" is unanswerable from a
            // capture without it, and an inactive path is exactly how a
            // display being turned off presents.
            tracelogging::write_event!(
                PROVIDER,
                "CommitModes2Path",
                level(Informational),
                u64("monitor", &(path.MonitorObject as u64)),
                u32("flags", &path.Flags),
                u32("active", &u32::from(is_active)),
                u32("width", &mode.map_or(0, |m| m.width)),
                u32("height", &mode.map_or(0, |m| m.height)),
                u32("refresh_mhz", &mode.map_or(0, |m| m.refresh_millihz))
            );
            let Some(mode) = mode else { continue };
            if !is_active || path.MonitorObject.is_null() {
                continue;
            }
            active_total = active_total.saturating_add(1);
            if active < MAX_COMMITTED_PATHS {
                recorded[active] = (path.MonitorObject as u64, mode);
                active += 1;
            } else {
                skipped += 1;
            }
        }
    }
    // REPLACE, not merge: this call carries the complete path set for the
    // adapter, so a monitor absent from it has nothing committed. Keeping a
    // stale entry would refuse that monitor's pushes forever.
    COMMITTED.apply(&recorded[..active], active_total);
    // Path-commit visibility (cold-boot instrumentation, 2026-07-25): a
    // monitor that never activates never gets a commit — this event's
    // absence after MonitorArrival localizes an activation failure to
    // the OS side of the mode negotiation in one trace.
    tracelogging::write_event!(
        PROVIDER,
        "CommitModes2",
        level(Informational),
        u32("paths", &count),
        u32("active", &(active as u32)),
        u32("skipped", &skipped),
        u64("generation", &COMMITTED.generation())
    );
    STATUS_SUCCESS
}

/// CAN_PROCESS_FP16 contract (IddCx 1.10): the OS provides the HDR 3x4
/// color matrix here for adapters that advertise FP16 processing. Our
/// pipeline is pass-through — ring consumers receive the composed FP16
/// scRGB desktop verbatim and the host encoder performs the colorspace
/// conversion — so the ramp/matrix is acknowledged and traced, not
/// applied to pixels. That matches physical-display streaming, where
/// capture happens before the scanout LUT as well (`caps::GAMMA_RAMP`
/// advertises the DDI, and GammaSupport is declared SOFTWARE).
pub unsafe extern "C" fn evt_monitor_set_gamma_ramp(
    monitor: ffi::IDDCX_MONITOR,
    in_args: *const ffi::IDARG_IN_SET_GAMMARAMP,
) -> NTSTATUS {
    let inp = &*in_args;
    let ramp_type = inp.Type as u32;
    tracelogging::write_event!(
        PROVIDER,
        "SetGammaRamp",
        level(Informational),
        u64("monitor", &(monitor as u64)),
        u32("type", &ramp_type),
        u32("size", &inp.GammaRampSizeInBytes)
    );
    STATUS_SUCCESS
}

pub unsafe extern "C" fn evt_set_default_hdr_metadata(
    monitor: ffi::IDDCX_MONITOR,
    in_args: *const ffi::IDARG_IN_MONITOR_SET_DEFAULT_HDR_METADATA,
) -> NTSTATUS {
    // The OS pushes the desktop's effective HDR10 static metadata here when
    // advanced color engages. The wire pixels already carry the composed
    // desktop (FP16 scRGB), so nothing needs reprocessing — trace it for
    // diagnostics; surfacing it to the host via GET_STATUS can come later
    // if the encoder wants monitor-derived mastering data.
    let inp = &*in_args;
    let meta_type = inp.Type as u32;
    tracelogging::write_event!(
        PROVIDER,
        "SetDefaultHdrMetaData",
        level(Informational),
        u64("monitor", &(monitor as u64)),
        u32("type", &meta_type),
        u32("size", &inp.Size)
    );
    STATUS_SUCCESS
}

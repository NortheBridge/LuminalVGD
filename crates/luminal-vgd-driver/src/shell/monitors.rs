// SPDX-License-Identifier: AGPL-3.0-only
//! Monitor plug/unplug via IddCx and the mode-list callbacks.
//!
//! Identity matching: EvtIddCxParseMonitorDescription receives only the
//! EDID (no monitor object), so sessions are found by the identity octets
//! our generator embeds — bytes 8..16 (vendor id, product code, serial).

use core::mem::{size_of, zeroed};

use wdk_sys::{NTSTATUS, STATUS_INVALID_PARAMETER, STATUS_SUCCESS};

use super::bindings::{self, ffi};
use super::PROVIDER;
use super::{MonitorRt, OsHandle, Shell};
use luminal_vgd_core::modes::Mode;
use luminal_vgd_core::session::ModeUpdateResult;

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

        // The ring section exists from plug time (state ACTIVE, no frames
        // yet), before the monitor arrives. Plug runs on the effects
        // worker shortly after the CREATE_MONITOR reply completes; the
        // host's ring open already retries on its own timeout budget.
        let ring = std::sync::Arc::new(super::swapchain::RingHandle::new(
            super::swapchain::FrameRing::new(session_id, ring_slots),
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

/// Stages for the `UpdateModes*` ETW events (constraint 4: every deny/fail
/// path carries stage + code). Numbering is append-only, like STAGE_* in
/// control.rs.
const UPD_STAGE_NO_MONITOR: u32 = 0; // session has no live or parked monitor
const UPD_STAGE_EMPTY: u32 = 1; // empty list — the DDI forbids count 0
const UPD_STAGE_PARKED: u32 = 2; // applied to a TDR-parked spec, no OS call
const UPD_STAGE_DUCK_PENDING: u32 = 3; // stored, OS push skipped (duck in flight)
const UPD_STAGE_OS_CALL: u32 = 4; // IddCxMonitorUpdateModes2 returned failure
const UPD_STAGE_ROLLBACK_RACED: u32 = 5; // monitor changed under a failed push
const UPD_STAGE_NO_ADAPTER: u32 = 8; // adapter torn down before the push (6/7 are control.rs's)
const UPD_STAGE_NOT_IN_SUPERSET: u32 = 9; // a target the live monitor cannot describe

/// What the OS push did with a pending target list. Every variant is
/// reported to `SessionTable::settle_modes`, and only `Applied` commits.
enum UpdatePush {
    /// `IddCxMonitorUpdateModes2` returned success: the list is in force.
    Applied { published: u32, superset: u32 },
    /// No OS call was made and nothing was left changed. RETRYABLE — the
    /// host's next identical request resolves and pushes for real.
    Deferred { stage: u32, count: u32 },
    /// The OS refused, or there was nothing to push at.
    Failed { stage: u32, code: i32, count: u32, rolled_back: bool },
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
    let push = push_targets(shell, session_id, targets);

    // Constraint 5: every deny/fail path carries stage + code.
    match push {
        UpdatePush::Applied { published, superset } => {
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
        UpdatePush::Deferred { stage, count } => {
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
                u32("retryable", &1u32)
            );
        }
        UpdatePush::Failed { stage, code, count, rolled_back } => {
            tracelogging::write_event!(
                PROVIDER,
                "UpdateModesFailed",
                level(Warning),
                u64("session", &session_id),
                u32("stage", &stage),
                i32("code", &code),
                u32("modes", &count),
                u32("rolled_back", &u32::from(rolled_back))
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
    let result = match push {
        UpdatePush::Applied { .. } => ModeUpdateResult::Applied,
        _ => ModeUpdateResult::NotApplied,
    };
    let settled = shell.dev.lock().unwrap().table.settle_modes(session_id, update_seq, result);
    if !settled {
        // The update was superseded by a newer one, or its session is
        // gone. Neither is an error — the newer update's own settle
        // covers a superset of these modes — but a silent miss and a
        // missing settle look identical in a capture, so say which.
        tracelogging::write_event!(
            PROVIDER,
            "UpdateModesSettleStale",
            level(Informational),
            u64("session", &session_id),
            u64("seq", &update_seq)
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
fn push_targets(shell: &Shell, session_id: u64, targets: Vec<Mode>) -> UpdatePush {
    // `IDARG_IN_UPDATEMODES2.TargetModeCount` "cannot be zero"
    // (IddCx.h:3594). The core layer refuses an empty selection before it
    // can ever become an effect, so this is the second gate, not the
    // first — but it is the one standing directly in front of the OS
    // call, and it must never be removed.
    if targets.is_empty() {
        return UpdatePush::Failed {
            stage: UPD_STAGE_EMPTY,
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
    // It is still a DEFERRAL, not an application: nothing has been
    // published to the OS, the replug may yet give up, and the durable
    // list must not claim otherwise. The patch and the retry converge —
    // a retry resolves from the durable superset to exactly this list.
    // The parked superset is the same one the request was validated
    // against (the replug recreates from the same EDID), so the subset
    // relation still holds; re-check anyway, because it is one compare
    // and the alternative is an unactivatable parked spec.
    {
        let mut ducked = shell.ducked.lock().unwrap();
        if let Some(d) = ducked.iter_mut().find(|d| d.session_id == session_id) {
            if targets.iter().all(|m| d.monitor_modes.contains(m)) {
                d.target_modes = targets;
                return UpdatePush::Deferred { stage: UPD_STAGE_PARKED, count };
            }
            return UpdatePush::Failed {
                stage: UPD_STAGE_NOT_IN_SUPERSET,
                code: luminal_driver_proto::err::BAD_MODE,
                count,
                rolled_back: false,
            };
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
    if shell.tdr_duck_pending.load(std::sync::atomic::Ordering::SeqCst) {
        return UpdatePush::Deferred { stage: UPD_STAGE_DUCK_PENDING, count };
    }
    // A final device exit (D3Final) clears the adapter, and the OS
    // destroys its child monitor objects with it.
    if shell.adapter().is_none() {
        return UpdatePush::Deferred { stage: UPD_STAGE_NO_ADAPTER, count };
    }

    // --- Under the lock: validate, publish, snapshot, build. No IddCx. ---
    let (monitor, previous, pushed, superset, target_modes) = {
        let mut monitors = shell.monitors.lock().unwrap();
        let Some(rt) = monitors.get_mut(&session_id) else {
            return UpdatePush::Failed {
                stage: UPD_STAGE_NO_MONITOR,
                code: luminal_driver_proto::err::NO_SUCH_SESSION,
                count,
                rolled_back: false,
            };
        };
        // THE containment check, against the LIVE monitor object's own
        // description rather than the session table's copy. The core
        // layer already filtered, but this is the last statement before
        // the OS call and the two lists could in principle have been
        // reconciled by different paths (a replug rebuilding the runtime,
        // a session id reused). Publishing a target the monitor cannot
        // describe would be invisible to the user and inexplicable in a
        // trace; refusing it costs one scan of at most four entries.
        if !targets.iter().all(|m| rt.monitor_modes.contains(m)) {
            return UpdatePush::Failed {
                stage: UPD_STAGE_NOT_IN_SUPERSET,
                code: luminal_driver_proto::err::BAD_MODE,
                count,
                rolled_back: false,
            };
        }
        let monitor = rt.monitor;
        let superset = rt.monitor_modes.len() as u32;
        let pushed = targets.clone();
        let previous = core::mem::replace(&mut rt.target_modes, targets);
        // monitor_modes is deliberately untouched: the EDID that
        // describes it cannot be reissued, and nothing here claims to
        // change what the monitor IS — only what it currently offers.
        let mut target_modes: Vec<ffi::IDDCX_TARGET_MODE2> =
            vec![unsafe { zeroed() }; rt.target_modes.len()];
        for (slot, mode) in target_modes.iter_mut().zip(rt.target_modes.iter()) {
            fill_target_mode2(slot, mode);
        }
        (monitor, previous, pushed, superset, target_modes)
    };

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
        return UpdatePush::Applied { published: count, superset };
    }

    // --- Failure: restore the previously published list. ---
    // Re-verify identity first (the AssignRacedUnplug pattern): between
    // dropping the guard and here, the session can have been destroyed,
    // reaped, ducked, or replugged onto a NEW monitor object — and
    // writing a stale list onto a fresh monitor would be worse than the
    // failure being rolled back.
    //
    // The list comparison is the second half of that check: the restore
    // only runs when the runtime list is still, entry for entry, the one
    // this call published, so it can only undo its own write and never a
    // later update's. What it puts back was itself a non-empty subset of
    // the same frozen superset, so the rollback cannot produce an
    // unactivatable or empty target list either.
    let mut rolled_back = false;
    let mut raced = false;
    {
        let mut monitors = shell.monitors.lock().unwrap();
        match monitors.get_mut(&session_id) {
            Some(rt) if rt.monitor == monitor && rt.target_modes == pushed => {
                rt.target_modes = previous;
                rolled_back = true;
            }
            _ => raced = true,
        }
    }
    UpdatePush::Failed {
        stage: if raced { UPD_STAGE_ROLLBACK_RACED } else { UPD_STAGE_OS_CALL },
        code: status,
        count,
        rolled_back,
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
    // keeps index 0 naming the same mode for the life of the monitor and
    // consistent with the frozen EDID's preferred detailed timing.
    out.MonitorModeBufferOutputCount = snapshot.monitor.len() as u32;
    out.PreferredMonitorModeIdx = 0;
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
    // `modes` here is the SUPERSET and is expected to be constant for the
    // life of the monitor — an `UPDATE_MODES` can never move it, so a
    // trace where it changes means something re-created the monitor.
    // `published` rides along so one line of a capture shows the gate
    // this monitor is currently under: published < modes means a subset
    // is being offered.
    tracelogging::write_event!(
        PROVIDER,
        "ParseDescription2",
        level(Informational),
        u32("modes", &(snapshot.monitor.len() as u32)),
        u32("published", &(snapshot.targets.len() as u32)),
        u32("buffer", &inp.MonitorModeBufferInputCount)
    );

    out.MonitorModeBufferOutputCount = snapshot.monitor.len() as u32;
    out.PreferredMonitorModeIdx = 0;
    if inp.MonitorModeBufferInputCount == 0 || inp.pMonitorModes.is_null() {
        return STATUS_SUCCESS;
    }
    let fill = snapshot.monitor.len().min(inp.MonitorModeBufferInputCount as usize);
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

pub unsafe extern "C" fn evt_commit_modes2(
    _adapter: ffi::IDDCX_ADAPTER,
    in_args: *const ffi::IDARG_IN_COMMITMODES2,
) -> NTSTATUS {
    // Path-commit visibility (cold-boot instrumentation, 2026-07-25): a
    // monitor that never activates never gets a commit — this event's
    // absence after MonitorArrival localizes an activation failure to
    // the OS side of the mode negotiation in one trace.
    let count = (*in_args).PathCount;
    tracelogging::write_event!(
        PROVIDER,
        "CommitModes2",
        level(Informational),
        u32("paths", &count)
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

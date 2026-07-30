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
pub fn plug(
    session_id: u64,
    display_id: u64,
    connector_index: u32,
    modes: Vec<Mode>,
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
        let ring = std::sync::Arc::new(std::sync::Mutex::new(
            super::swapchain::FrameRing::new(session_id, ring_slots),
        ));
        let displaced = shell.monitors.lock().unwrap().insert(
            session_id,
            MonitorRt {
                monitor: OsHandle(monitor.cast()),
                edid,
                modes,
                display_id,
                connector_index,
                adapter_luid,
                assign_seq: 0,
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
    let shell = super::Shell::get();
    let drained: Vec<(u64, super::MonitorRt)> = {
        let mut monitors = shell.monitors.lock().unwrap();
        monitors.drain().collect()
    };
    let mut parked = 0usize;
    for (session_id, mut rt) in drained {
        if let Some(worker) = rt.worker.take() {
            worker.stop();
        }
        if let Some(cursor) = rt.cursor.as_mut() {
            cursor.stop();
        }
        // Single bounded attempt: a detached worker may pin the ring
        // mutex, and unlike mark_ring_dead there is no urgency to win —
        // the host's stale-heartbeat detection covers an unmarked ring.
        if let Ok(ring) = rt.ring.try_lock() {
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
            modes: rt.modes,
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
            let displaced = shell.monitors.lock().unwrap().insert(
                d.session_id,
                super::MonitorRt {
                    monitor: super::OsHandle(monitor.cast()),
                    edid: d.edid,
                    modes: d.modes,
                    display_id: d.display_id,
                    connector_index: d.connector_index,
                    adapter_luid: d.adapter_luid,
                    assign_seq: 0,
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

/// Find the session whose EDID identity octets match `desc` bytes 8..16.
fn session_modes_for_edid(data: &[u8]) -> Option<Vec<Mode>> {
    if data.len() < 128 {
        return None;
    }
    let shell = Shell::get();
    let monitors = shell.monitors.lock().unwrap();
    monitors
        .values()
        .find(|rt| rt.edid[8..16] == data[8..16])
        .map(|rt| rt.modes.clone())
}

fn modes_for_monitor_object(monitor: ffi::IDDCX_MONITOR) -> Option<Vec<Mode>> {
    let shell = Shell::get();
    let monitors = shell.monitors.lock().unwrap();
    monitors
        .values()
        .find(|rt| rt.monitor == OsHandle(monitor.cast()))
        .map(|rt| rt.modes.clone())
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
    let Some(modes) = session_modes_for_edid(data) else {
        return STATUS_INVALID_PARAMETER;
    };

    out.MonitorModeBufferOutputCount = modes.len() as u32;
    out.PreferredMonitorModeIdx = 0;
    if inp.MonitorModeBufferInputCount == 0 || inp.pMonitorModes.is_null() {
        return STATUS_SUCCESS;
    }
    let fill = modes.len().min(inp.MonitorModeBufferInputCount as usize);
    let slots = core::slice::from_raw_parts_mut(inp.pMonitorModes, fill);
    for (slot, mode) in slots.iter_mut().zip(modes.iter()) {
        slot.Size = size_of::<ffi::IDDCX_MONITOR_MODE>() as u32;
        slot.Origin = ffi::IDDCX_MONITOR_MODE_ORIGIN_IDDCX_MONITOR_MODE_ORIGIN_MONITORDESCRIPTOR;
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
    let Some(modes) = modes_for_monitor_object(monitor) else {
        return STATUS_INVALID_PARAMETER;
    };

    out.TargetModeBufferOutputCount = modes.len() as u32;
    if inp.TargetModeBufferInputCount == 0 || inp.pTargetModes.is_null() {
        return STATUS_SUCCESS;
    }
    let fill = modes.len().min(inp.TargetModeBufferInputCount as usize);
    let slots = core::slice::from_raw_parts_mut(inp.pTargetModes, fill);
    for (slot, mode) in slots.iter_mut().zip(modes.iter()) {
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
    let Some(modes) = session_modes_for_edid(data) else {
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
    tracelogging::write_event!(
        PROVIDER,
        "ParseDescription2",
        level(Informational),
        u32("modes", &(modes.len() as u32)),
        u32("buffer", &inp.MonitorModeBufferInputCount)
    );

    out.MonitorModeBufferOutputCount = modes.len() as u32;
    out.PreferredMonitorModeIdx = 0;
    if inp.MonitorModeBufferInputCount == 0 || inp.pMonitorModes.is_null() {
        return STATUS_SUCCESS;
    }
    let fill = modes.len().min(inp.MonitorModeBufferInputCount as usize);
    let slots = core::slice::from_raw_parts_mut(inp.pMonitorModes, fill);
    for (slot, mode) in slots.iter_mut().zip(modes.iter()) {
        slot.Size = size_of::<ffi::IDDCX_MONITOR_MODE2>() as u32;
        slot.Origin = ffi::IDDCX_MONITOR_MODE_ORIGIN_IDDCX_MONITOR_MODE_ORIGIN_MONITORDESCRIPTOR;
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
    let Some(modes) = modes_for_monitor_object(monitor) else {
        tracelogging::write_event!(
            PROVIDER,
            "QueryTargetModes2NoSession",
            level(Warning),
            u64("monitor_ptr", &(monitor as u64))
        );
        return STATUS_INVALID_PARAMETER;
    };
    tracelogging::write_event!(
        PROVIDER,
        "QueryTargetModes2",
        level(Informational),
        u32("modes", &(modes.len() as u32)),
        u32("buffer", &inp.TargetModeBufferInputCount)
    );

    out.TargetModeBufferOutputCount = modes.len() as u32;
    if inp.TargetModeBufferInputCount == 0 || inp.pTargetModes.is_null() {
        return STATUS_SUCCESS;
    }
    let fill = modes.len().min(inp.TargetModeBufferInputCount as usize);
    let slots = core::slice::from_raw_parts_mut(inp.pTargetModes, fill);
    for (slot, mode) in slots.iter_mut().zip(modes.iter()) {
        slot.Size = size_of::<ffi::IDDCX_TARGET_MODE2>() as u32;
        slot.TargetVideoSignalInfo.targetVideoSignalInfo = signal_info(mode, 1);
        // Zero, matching MaxDisplayPipelineRate = 0: bandwidth management
        // unused. A nonzero requirement against a zero adapter budget makes
        // every mode unactivatable (Extend reverts, Scale/Res grayed).
        slot.RequiredBandwidth = 0;
        slot.BitsPerComponent = wire_bpc_for(mode);
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

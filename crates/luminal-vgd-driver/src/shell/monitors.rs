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
    _adapter_luid: u64,
    ring_slots: u32,
    edid: Box<[u8; 256]>,
) {
    let shell = Shell::get();
    let Some(adapter) = shell.adapter.get().copied() else {
        tracelogging::write_event!(PROVIDER, "PlugBeforeAdapterReady", level(Error));
        return;
    };

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
        // yet) so the host can map it as soon as CREATE_MONITOR replies.
        let ring = std::sync::Arc::new(std::sync::Mutex::new(
            super::swapchain::FrameRing::new(session_id, ring_slots),
        ));
        shell.monitors.lock().unwrap().insert(
            session_id,
            MonitorRt {
                monitor: OsHandle(monitor.cast()),
                edid,
                modes,
                worker: None,
                ring,
            },
        );

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
        }
    }
}

/// Unplug: stop the frame worker (bounded), mark the ring DEAD so the
/// host unmaps, then IddCxMonitorDeparture.
pub fn unplug(session_id: u64) {
    let shell = Shell::get();
    let Some(mut rt) = shell.monitors.lock().unwrap().remove(&session_id) else {
        return;
    };
    if let Some(worker) = rt.worker.take() {
        worker.stop();
    }
    if let Ok(ring) = rt.ring.lock() {
        if let Some(section) = &ring.section {
            section.set_state(luminal_driver_proto::ring_state::DEAD);
        }
    }
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

/// SDR-only wire format: 8-bit RGB, nothing else.
fn wire_bpc_sdr8() -> ffi::IDDCX_WIRE_BITS_PER_COMPONENT {
    let mut bpc: ffi::IDDCX_WIRE_BITS_PER_COMPONENT = unsafe { zeroed() };
    bpc.Rgb = ffi::IDDCX_BITS_PER_COMPONENT_IDDCX_BITS_PER_COMPONENT_8;
    bpc
}

pub unsafe extern "C" fn evt_adapter_query_target_info(
    _adapter: ffi::IDDCX_ADAPTER,
    _in_args: *mut ffi::IDARG_IN_QUERYTARGET_INFO,
    out_args: *mut ffi::IDARG_OUT_QUERYTARGET_INFO,
) -> NTSTATUS {
    let out = &mut *out_args;
    out.TargetCaps = ffi::IDDCX_TARGET_CAPS_IDDCX_TARGET_CAPS_NONE;
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
        slot.Size = size_of::<ffi::IDDCX_MONITOR_MODE2>() as u32;
        slot.Origin = ffi::IDDCX_MONITOR_MODE_ORIGIN_IDDCX_MONITOR_MODE_ORIGIN_MONITORDESCRIPTOR;
        slot.MonitorVideoSignalInfo = signal_info(mode, 0);
        slot.BitsPerComponent = wire_bpc_sdr8();
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
        return STATUS_INVALID_PARAMETER;
    };

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
        slot.BitsPerComponent = wire_bpc_sdr8();
    }
    out.TargetModeBufferOutputCount = fill as u32;
    STATUS_SUCCESS
}

pub unsafe extern "C" fn evt_commit_modes2(
    _adapter: ffi::IDDCX_ADAPTER,
    _in_args: *const ffi::IDARG_IN_COMMITMODES2,
) -> NTSTATUS {
    STATUS_SUCCESS
}

pub unsafe extern "C" fn evt_set_default_hdr_metadata(
    _monitor: ffi::IDDCX_MONITOR,
    _in_args: *const ffi::IDARG_IN_MONITOR_SET_DEFAULT_HDR_METADATA,
) -> NTSTATUS {
    // SDR-only shell: accept and ignore. Stored + used when caps::HDR10
    // and the phase-4 frame pipeline land.
    STATUS_SUCCESS
}

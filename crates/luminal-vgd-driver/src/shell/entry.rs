// SPDX-License-Identifier: AGPL-3.0-only
//! DriverEntry → device add → adapter bring-up → watchdog timer.

use core::mem::{size_of, zeroed};
use core::ptr::{null_mut, addr_of_mut};
use std::sync::Mutex;

use wdk_sys::{
    call_unsafe_wdf_function_binding, NTSTATUS, PCUNICODE_STRING, PDRIVER_OBJECT,
    PWDFDEVICE_INIT, STATUS_SUCCESS, WDFDEVICE, WDFDRIVER, WDFTIMER, WDF_DRIVER_CONFIG,
    WDF_FILEOBJECT_CONFIG, WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES, WDF_OBJECT_ATTRIBUTES,
    WDF_PNPPOWER_EVENT_CALLBACKS, WDF_POWER_DEVICE_STATE, WDF_TIMER_CONFIG, GUID,
    _WDF_EXECUTION_LEVEL::WdfExecutionLevelInheritFromParent,
    _WDF_SYNCHRONIZATION_SCOPE::WdfSynchronizationScopeInheritFromParent,
};

use super::bindings::{self, ffi};
use super::{control, monitors, OsHandle, Shell, DRIVER_BUILD, PROVIDER, SHELL_CAPS};
use crate::dispatch::{watchdog_tick, DeviceState, DriverConfig};

fn interface_guid() -> GUID {
    let (d1, d2, d3, d4) = luminal_driver_proto::LUMINAL_VGD_INTERFACE_GUID;
    GUID { Data1: d1, Data2: d2, Data3: d3, Data4: d4 }
}

// SAFETY: the required export for the UMDF stub; nothing else in this
// compilation unit exports the name.
#[export_name = "DriverEntry"]
pub unsafe extern "system" fn driver_entry(
    driver: PDRIVER_OBJECT,
    registry_path: PCUNICODE_STRING,
) -> NTSTATUS {
    PROVIDER.register();
    // First-instruction breadcrumb: proves the DLL loaded and DriverEntry
    // ran when diagnosing load/start failures from an ETW capture.
    tracelogging::write_event!(PROVIDER, "DriverEntry", level(Informational));

    let mut config: WDF_DRIVER_CONFIG = zeroed();
    config.Size = size_of::<WDF_DRIVER_CONFIG>() as u32;
    config.EvtDriverDeviceAdd = Some(evt_device_add);

    call_unsafe_wdf_function_binding!(
        WdfDriverCreate,
        driver,
        registry_path,
        WDF_NO_OBJECT_ATTRIBUTES,
        &mut config,
        WDF_NO_HANDLE.cast()
    )
}

unsafe extern "C" fn evt_device_add(
    _driver: WDFDRIVER,
    device_init: PWDFDEVICE_INIT,
) -> NTSTATUS {
    tracelogging::write_event!(PROVIDER, "DeviceAddEnter", level(Informational));
    let mut pnp: WDF_PNPPOWER_EVENT_CALLBACKS = zeroed();
    pnp.Size = size_of::<WDF_PNPPOWER_EVENT_CALLBACKS>() as u32;
    pnp.EvtDeviceD0Entry = Some(evt_d0_entry);
    pnp.EvtDeviceD0Exit = Some(evt_d0_exit);
    call_unsafe_wdf_function_binding!(WdfDeviceInitSetPnpPowerEventCallbacks, device_init, &mut pnp);

    // Per-handle contexts (handshake gating) ride on file objects.
    let mut foc: WDF_FILEOBJECT_CONFIG = zeroed();
    foc.Size = size_of::<WDF_FILEOBJECT_CONFIG>() as u32;
    foc.EvtDeviceFileCreate = Some(control::evt_file_create);
    foc.EvtFileClose = Some(control::evt_file_close);
    call_unsafe_wdf_function_binding!(
        WdfDeviceInitSetFileObjectConfig,
        device_init,
        &mut foc,
        WDF_NO_OBJECT_ATTRIBUTES
    );

    let mut idd: ffi::IDD_CX_CLIENT_CONFIG = zeroed();
    idd.Size = size_of::<ffi::IDD_CX_CLIENT_CONFIG>() as u32;
    idd.EvtIddCxDeviceIoControl = Some(control::evt_ioctl);
    idd.EvtIddCxParseMonitorDescription = Some(monitors::evt_parse_monitor_description);
    idd.EvtIddCxAdapterInitFinished = Some(evt_adapter_init_finished);
    idd.EvtIddCxAdapterCommitModes = Some(monitors::evt_commit_modes);
    idd.EvtIddCxMonitorGetDefaultDescriptionModes = Some(monitors::evt_default_modes);
    idd.EvtIddCxMonitorQueryTargetModes = Some(monitors::evt_query_target_modes);
    idd.EvtIddCxMonitorAssignSwapChain = Some(super::swapchain::evt_assign);
    idd.EvtIddCxMonitorUnassignSwapChain = Some(super::swapchain::evt_unassign);
    // Mandatory for IddCx ≥1.4 clients (missing ⇒ device start fails
    // with STATUS_DEVICE_CONFIGURATION_ERROR).
    idd.EvtIddCxParseMonitorDescription2 = Some(monitors::evt_parse_monitor_description2);
    idd.EvtIddCxAdapterQueryTargetInfo = Some(monitors::evt_adapter_query_target_info);
    idd.EvtIddCxAdapterCommitModes2 = Some(monitors::evt_commit_modes2);
    idd.EvtIddCxMonitorSetDefaultHdrMetaData = Some(monitors::evt_set_default_hdr_metadata);
    idd.EvtIddCxMonitorQueryTargetModes2 = Some(monitors::evt_query_target_modes2);
    // CAN_PROCESS_FP16 obligates the gamma-ramp DDI (HDR 3x4 matrix);
    // an FP16 adapter without it fails AdapterInitAsync validation.
    idd.EvtIddCxMonitorSetGammaRamp = Some(monitors::evt_monitor_set_gamma_ramp);

    let status = bindings::device_init_config(device_init, &idd);
    if status != STATUS_SUCCESS {
        return status;
    }

    let mut device: WDFDEVICE = null_mut();
    let mut init = device_init;
    let status = call_unsafe_wdf_function_binding!(
        WdfDeviceCreate,
        &mut init,
        WDF_NO_OBJECT_ATTRIBUTES,
        &mut device
    );
    if status != STATUS_SUCCESS {
        return status;
    }

    let status = bindings::device_initialize(device);
    if status != STATUS_SUCCESS {
        return status;
    }

    // The control interface the host enumerates (proto GUID), registered
    // WITH a reference string: opens through the interface symlink then
    // carry "\LuminalVGDControl" as the file name, which is what lets
    // EvtDeviceFileCreate tell control-plane opens apart from the OS
    // graphics stack's own (unelevated, name-less) opens of the same
    // device object. The DESIGN.md §6 SYSTEM+Admins ACL is enforced there
    // — a device-wide SDDL would break IddCx (phase-2 lesson).
    let guid = interface_guid();
    let ref_string = control::control_ref_unicode();
    let status = call_unsafe_wdf_function_binding!(
        WdfDeviceCreateDeviceInterface,
        device,
        &guid,
        &ref_string
    );
    if status != STATUS_SUCCESS {
        return status;
    }

    // Portable state: config defaults + persisted blob (identity
    // reservations, permanent pool) from the device registry key.
    let persisted = control::read_persisted(device);
    let cfg = DriverConfig {
        caps: SHELL_CAPS,
        driver_build: DRIVER_BUILD,
        ..DriverConfig::default()
    };
    let shell = Shell::init(DeviceState::new(cfg, persisted.as_deref()));
    shell.set_wdf_device(OsHandle(device.cast()));

    // 1 s periodic watchdog (feeds dispatch::watchdog_tick).
    let mut tc: WDF_TIMER_CONFIG = zeroed();
    tc.Size = size_of::<WDF_TIMER_CONFIG>() as u32;
    tc.EvtTimerFunc = Some(evt_watchdog_tick);
    tc.Period = 1000;
    let mut attrs: WDF_OBJECT_ATTRIBUTES = zeroed();
    attrs.Size = size_of::<WDF_OBJECT_ATTRIBUTES>() as u32;
    attrs.ExecutionLevel = WdfExecutionLevelInheritFromParent;
    attrs.SynchronizationScope = WdfSynchronizationScopeInheritFromParent;
    attrs.ParentObject = device.cast();
    let mut timer: WDFTIMER = null_mut();
    let status =
        call_unsafe_wdf_function_binding!(WdfTimerCreate, &mut tc, &mut attrs, &mut timer);
    if status != STATUS_SUCCESS {
        return status;
    }
    // Relative due time, 100 ns units (negative = relative). The return
    // value only says whether the timer was already queued.
    let _ = call_unsafe_wdf_function_binding!(WdfTimerStart, timer, -10_000_000i64);

    tracelogging::write_event!(PROVIDER, "DeviceAdd", level(Informational));
    STATUS_SUCCESS
}

/// Which WDFDEVICE the adapter has been brought up for, plus the adapter
/// epoch captured when IddCxAdapterInitAsync was issued. Same-device D0
/// re-entries (sleep/resume) skip re-init — the IddSampleDriver pattern —
/// but a DIFFERENT device (same-process devnode re-add after removal)
/// re-arms bring-up. The process-global one-shot this replaces skipped
/// the second device silently, leaving the OS waiting on an indirect
/// adapter that would never initialize. The epoch is captured HERE (not
/// in EvtIddCxAdapterInitFinished) so an InitFinished delivered after a
/// D3Final teardown carries the pre-teardown epoch and goes stale
/// instead of republishing a destroyed adapter.
/// Singleton assumption: exactly one live LuminalVGD devnode (INF
/// root-enumerated singleton) — PnP serializes remove-before-re-add, which
/// is what makes the single global mark and the D0Entry epoch capture
/// sound. A multi-devnode design would need per-device marks.
struct StartMark {
    device: usize,
    /// IDDCX_ADAPTER returned by IddCxAdapterInitAsync; 0 while init is
    /// still in flight. Ties EvtIddCxAdapterInitFinished to the device
    /// that issued it, so a late callback for a torn-down device cannot
    /// borrow a replacement device's mark.
    adapter: usize,
    epoch: u64,
}

static STARTED_FOR: Mutex<Option<StartMark>> = Mutex::new(None);

/// Static endpoint diagnostics (telemetry-only per the header).
static MODEL: [u16; 11] = super::wide("LuminalVGD");
static MANUFACTURER: [u16; 24] = super::wide("NortheBridge Foundation");
static FRIENDLY: [u16; 31] = super::wide("Luminal Video Graphics Display");

unsafe extern "C" fn evt_d0_entry(
    device: WDFDEVICE,
    _previous_state: WDF_POWER_DEVICE_STATE,
) -> NTSTATUS {
    {
        let epoch = Shell::get().adapter_epoch();
        let mut started = STARTED_FOR.lock().unwrap();
        if started.as_ref().is_some_and(|m| m.device == device as usize) {
            return STATUS_SUCCESS;
        }
        *started = Some(StartMark { device: device as usize, adapter: 0, epoch });
    }

    let mut version: ffi::IDDCX_ENDPOINT_VERSION = zeroed();
    version.Size = size_of::<ffi::IDDCX_ENDPOINT_VERSION>() as u32;
    version.MajorVer = 1;
    version.Build = DRIVER_BUILD;

    let mut caps: ffi::IDDCX_ADAPTER_CAPS = zeroed();
    caps.Size = size_of::<ffi::IDDCX_ADAPTER_CAPS>() as u32;
    caps.MaxMonitorsSupported = luminal_driver_proto::DEFAULT_MAX_MONITORS;
    // HDR10: the OS only offers advanced color on an indirect display when
    // the adapter declares it can process FP16 desktop content; the frame
    // pipeline is format-agnostic (ring textures follow the acquired
    // frame's desc), so this is safe to declare unconditionally.
    caps.Flags = ffi::IDDCX_ADAPTER_FLAGS_IDDCX_ADAPTER_FLAGS_CAN_PROCESS_FP16;
    // MaxDisplayPipelineRate stays 0 (IddSampleDriver convention);
    // u64::MAX fails IddCxAdapterInitAsync parameter validation.
    caps.EndPointDiagnostics.Size = size_of::<ffi::IDDCX_ENDPOINT_DIAGNOSTIC_INFO>() as u32;
    caps.EndPointDiagnostics.TransmissionType =
        ffi::IDDCX_TRANSMISSION_TYPE_IDDCX_TRANSMISSION_TYPE_WIRED_OTHER;
    caps.EndPointDiagnostics.pEndPointFriendlyName = FRIENDLY.as_ptr();
    caps.EndPointDiagnostics.pEndPointModelName = MODEL.as_ptr();
    caps.EndPointDiagnostics.pEndPointManufacturerName = MANUFACTURER.as_ptr();
    caps.EndPointDiagnostics.pHardwareVersion = addr_of_mut!(version);
    caps.EndPointDiagnostics.pFirmwareVersion = addr_of_mut!(version);
    // SOFTWARE, matching the registered EvtIddCxMonitorSetGammaRamp: the
    // HDR 3x4 matrix is acknowledged by the driver (conversion happens in
    // the host encoder). NONE alongside CAN_PROCESS_FP16 is contradictory.
    caps.EndPointDiagnostics.GammaSupport = ffi::IDDCX_FEATURE_IMPLEMENTATION_IDDCX_FEATURE_IMPLEMENTATION_SOFTWARE;

    let mut in_args: ffi::IDARG_IN_ADAPTER_INIT = zeroed();
    in_args.WdfDevice = device;
    in_args.pCaps = &mut caps;
    let mut out_args: ffi::IDARG_OUT_ADAPTER_INIT = zeroed();
    let status = bindings::adapter_init_async(&in_args, &mut out_args);
    tracelogging::write_event!(
        PROVIDER,
        "AdapterInitAsync",
        level(Informational),
        i32("status", &status)
    );
    let mut started = STARTED_FOR.lock().unwrap();
    if status != STATUS_SUCCESS {
        // Failed D0Entry gets no D0Exit from WDF — roll the marker back
        // here or a retry on this device could never re-arm bring-up.
        if started.as_ref().is_some_and(|m| m.device == device as usize) {
            *started = None;
        }
    } else if let Some(mark) =
        started.as_mut().filter(|m| m.device == device as usize)
    {
        // Record the adapter so InitFinished can prove the callback is
        // ours (InitFinished may race this store; it accepts adapter==0
        // as "init in flight on this device").
        mark.adapter = out_args.AdapterObject as usize;
    }
    status
}

unsafe extern "C" fn evt_adapter_init_finished(
    adapter: ffi::IDDCX_ADAPTER,
    in_args: *const ffi::IDARG_IN_ADAPTER_INIT_FINISHED,
) -> NTSTATUS {
    let init_status = (*in_args).AdapterInitStatus;
    if init_status < 0 {
        tracelogging::write_event!(
            PROVIDER,
            "AdapterInitFailed",
            level(Error),
            i32("status", &init_status)
        );
        return STATUS_SUCCESS;
    }

    // Everything heavy is deferred to the effects worker: the DXGI walk
    // is D3D work and the permanent-pool replug calls into IddCx, and
    // neither may run on this win32k-callout frame (builds 6/7 rule).
    // The worker publishes ready() in the same order the inline version
    // did (adapters + startup first). The epoch comes from the D0Entry
    // capture (StartMark), so an InitFinished arriving after a D3Final
    // teardown is provably stale; the adapter-identity check keeps a
    // late callback for a torn-down device from borrowing a replacement
    // device's mark (0 = the D0Entry-side store may still be in flight).
    // A missing/mismatched mark means nothing to bring up.
    let epoch = {
        let started = STARTED_FOR.lock().unwrap();
        started.as_ref().and_then(|m| {
            (m.adapter == adapter as usize || m.adapter == 0).then_some(m.epoch)
        })
    };
    let Some(epoch) = epoch else {
        tracelogging::write_event!(PROVIDER, "AdapterReadyStale", level(Warning));
        return STATUS_SUCCESS;
    };
    control::queue_adapter_ready(OsHandle(adapter.cast()), epoch);
    STATUS_SUCCESS
}

/// Final-exit teardown. Only D3Final (device stop/remove) tears down:
/// stop every worker under its bounded deadline, mark rings DEAD so
/// hosts unmap, forget the monitor objects (the OS destroys them with
/// the adapter — no IddCxMonitorDeparture from a power callback), and
/// clear the adapter so a replacement device re-arms bring-up. Sleep
/// transitions keep all state: the OS unassigns swap chains around Dx
/// itself and the monitors persist across resume.
unsafe extern "C" fn evt_d0_exit(
    device: WDFDEVICE,
    target_state: WDF_POWER_DEVICE_STATE,
) -> NTSTATUS {
    if target_state != wdk_sys::_WDF_POWER_DEVICE_STATE::WdfPowerDeviceD3Final {
        return STATUS_SUCCESS;
    }
    let Some(shell) = Shell::try_get() else { return STATUS_SUCCESS };
    tracelogging::write_event!(PROVIDER, "DeviceFinalExit", level(Informational));
    // Clear FIRST, before the (multi-second worst case) drain below:
    // clear_adapter bumps the adapter epoch, so a bring-up racing this
    // teardown deterministically takes its stale path instead of
    // publishing the doomed adapter and plugging monitors into a map we
    // already drained; clearing the WDF device keeps a queued
    // PersistState from touching a destroyed WDFDEVICE handle.
    shell.clear_adapter();
    shell.clear_wdf_device();
    let drained: Vec<(u64, super::MonitorRt)> = {
        let mut monitors = shell.monitors.lock().unwrap();
        monitors.drain().collect()
    };
    for (session_id, mut rt) in drained {
        if let Some(worker) = rt.worker.take() {
            worker.stop();
        }
        if let Some(cursor) = rt.cursor.as_mut() {
            cursor.stop();
        }
        // Bounded (a detached worker may still pin the ring mutex).
        rt.mark_ring_dead();
        tracelogging::write_event!(
            PROVIDER,
            "MonitorTornDown",
            level(Informational),
            u64("session", &session_id)
        );
    }
    // Portable-state reconciliation: every monitor runtime is gone, so
    // drop every table session — lease-disabled pool members included
    // (the watchdog can never reap those) — while the desired pool
    // config stays. Without this a replacement device's startup() hits
    // DuplicateSession and bricks the pool.
    shell.dev.lock().unwrap().device_teardown_reset();
    let mut started = STARTED_FOR.lock().unwrap();
    if started.as_ref().is_some_and(|m| m.device == device as usize) {
        *started = None;
    }
    STATUS_SUCCESS
}

unsafe extern "C" fn evt_watchdog_tick(_timer: WDFTIMER) {
    let Some(shell) = Shell::try_get() else { return };
    if !shell.ready() {
        return;
    }
    let effects = {
        let mut dev = shell.dev.lock().unwrap();
        watchdog_tick(&mut dev, shell.now_ms())
    };
    // Queued, not applied: lease-reap unplugs join workers and call
    // IddCxMonitorDeparture, which must not run on a WDF timer frame.
    control::queue_effects(effects);
}

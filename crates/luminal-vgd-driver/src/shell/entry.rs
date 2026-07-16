// SPDX-License-Identifier: AGPL-3.0-only
//! DriverEntry → device add → adapter bring-up → watchdog timer.

use core::mem::{size_of, zeroed};
use core::ptr::{null_mut, addr_of_mut};
use std::sync::atomic::{AtomicBool, Ordering};

use wdk_sys::{
    call_unsafe_wdf_function_binding, NTSTATUS, PCUNICODE_STRING, PDRIVER_OBJECT,
    PWDFDEVICE_INIT, STATUS_SUCCESS, WDFDEVICE, WDFDRIVER, WDFTIMER, WDF_DRIVER_CONFIG,
    WDF_FILEOBJECT_CONFIG, WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES, WDF_OBJECT_ATTRIBUTES,
    WDF_PNPPOWER_EVENT_CALLBACKS, WDF_POWER_DEVICE_STATE, WDF_TIMER_CONFIG, GUID,
    _WDF_EXECUTION_LEVEL::WdfExecutionLevelInheritFromParent,
    _WDF_SYNCHRONIZATION_SCOPE::WdfSynchronizationScopeInheritFromParent,
};

use super::bindings::{self, ffi};
use super::{control, dxgi, monitors, OsHandle, Shell, DRIVER_BUILD, PROVIDER, SHELL_CAPS};
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

    // The control interface the host enumerates (proto GUID). Device
    // object security (SYSTEM+Admins SDDL) is enforced from the INF.
    let guid = interface_guid();
    let status = call_unsafe_wdf_function_binding!(
        WdfDeviceCreateDeviceInterface,
        device,
        &guid,
        null_mut()
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
    let _ = shell.wdf_device.set(OsHandle(device.cast()));

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

/// Adapter bring-up happens on first D0 entry (IddSampleDriver pattern).
static ADAPTER_STARTED: AtomicBool = AtomicBool::new(false);

/// Static endpoint diagnostics (telemetry-only per the header).
static MODEL: [u16; 11] = super::wide("LuminalVGD");
static MANUFACTURER: [u16; 24] = super::wide("NortheBridge Foundation");
static FRIENDLY: [u16; 31] = super::wide("Luminal Video Graphics Display");

unsafe extern "C" fn evt_d0_entry(
    device: WDFDEVICE,
    _previous_state: WDF_POWER_DEVICE_STATE,
) -> NTSTATUS {
    if ADAPTER_STARTED.swap(true, Ordering::SeqCst) {
        return STATUS_SUCCESS;
    }

    let mut version: ffi::IDDCX_ENDPOINT_VERSION = zeroed();
    version.Size = size_of::<ffi::IDDCX_ENDPOINT_VERSION>() as u32;
    version.MajorVer = 1;
    version.Build = DRIVER_BUILD;

    let mut caps: ffi::IDDCX_ADAPTER_CAPS = zeroed();
    caps.Size = size_of::<ffi::IDDCX_ADAPTER_CAPS>() as u32;
    caps.MaxMonitorsSupported = luminal_driver_proto::DEFAULT_MAX_MONITORS;
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
    caps.EndPointDiagnostics.GammaSupport = ffi::IDDCX_FEATURE_IMPLEMENTATION_IDDCX_FEATURE_IMPLEMENTATION_NONE;

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

    let shell = Shell::get();
    let adapters = dxgi::enumerate();
    let effects = {
        let mut dev = shell.dev.lock().unwrap();
        dev.set_adapters(adapters);
        dev.startup(shell.now_ms())
    };
    let _ = shell.adapter.set(OsHandle(adapter.cast()));
    // Recreate persisted permanent-pool displays, outside the lock.
    control::apply_effects(effects);
    tracelogging::write_event!(PROVIDER, "AdapterReady", level(Informational));
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
    if !effects.is_empty() {
        control::apply_effects(effects);
    }
}

// SPDX-License-Identifier: AGPL-3.0-only
//! IddCx FFI: bindgen output from the eWDK's IddCx.h plus thin call
//! wrappers replicating the header's function-table dispatch (the C
//! inline shims cast `IddFunctions[Index]` to the right PFN and prepend
//! `IddDriverGlobals`; both symbols live in iddcxstub.lib).

#[allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code,
    clippy::all
)]
pub mod ffi {
    include!(concat!(env!("OUT_DIR"), "/iddcx.rs"));

    extern "C" {
        /// Declared with a generous fixed size because Rust externs cannot
        /// be unsized arrays; only ever indexed via the *TableIndex enum,
        /// which the stub guarantees is in range for the bound version.
        pub static IddFunctions: [PFN_IDD_CX; 1024];
    }
}

use ffi::*;
use wdk_sys::{NTSTATUS, PWDFDEVICE_INIT, WDFDEVICE};

/// The IddCx stub links against this client-provided global (the C header
/// emits it via `__declspec(selectany)`; bindgen only declares it). Must
/// match the minor of the IddCx headers build.rs compiles against (1.10).
#[no_mangle]
static IddMinimumVersionRequired: u32 = 10;

macro_rules! iddcx_call {
    ($index:ident as $pfn:ty, $($arg:expr),*) => {{
        let f: $pfn = core::mem::transmute(IddFunctions[$index as usize]);
        (f.expect("IddCx function table entry"))(IddDriverGlobals, $($arg),*)
    }};
}

pub unsafe fn device_init_config(
    init: PWDFDEVICE_INIT,
    config: *const IDD_CX_CLIENT_CONFIG,
) -> NTSTATUS {
    iddcx_call!(_IDDFUNCENUM_IddCxDeviceInitConfigTableIndex as PFN_IDDCXDEVICEINITCONFIG, init, config)
}

pub unsafe fn device_initialize(device: WDFDEVICE) -> NTSTATUS {
    iddcx_call!(_IDDFUNCENUM_IddCxDeviceInitializeTableIndex as PFN_IDDCXDEVICEINITIALIZE, device)
}

pub unsafe fn adapter_init_async(
    in_args: *const IDARG_IN_ADAPTER_INIT,
    out_args: *mut IDARG_OUT_ADAPTER_INIT,
) -> NTSTATUS {
    iddcx_call!(_IDDFUNCENUM_IddCxAdapterInitAsyncTableIndex as PFN_IDDCXADAPTERINITASYNC, in_args, out_args)
}

pub unsafe fn monitor_create(
    adapter: IDDCX_ADAPTER,
    in_args: *const IDARG_IN_MONITORCREATE,
    out_args: *mut IDARG_OUT_MONITORCREATE,
) -> NTSTATUS {
    iddcx_call!(_IDDFUNCENUM_IddCxMonitorCreateTableIndex as PFN_IDDCXMONITORCREATE, adapter, in_args, out_args)
}

pub unsafe fn monitor_arrival(
    monitor: IDDCX_MONITOR,
    out_args: *mut IDARG_OUT_MONITORARRIVAL,
) -> NTSTATUS {
    iddcx_call!(_IDDFUNCENUM_IddCxMonitorArrivalTableIndex as PFN_IDDCXMONITORARRIVAL, monitor, out_args)
}

pub unsafe fn monitor_departure(monitor: IDDCX_MONITOR) -> NTSTATUS {
    iddcx_call!(_IDDFUNCENUM_IddCxMonitorDepartureTableIndex as PFN_IDDCXMONITORDEPARTURE, monitor)
}

/// Push a new TARGET-mode list for a monitor that is already ARRIVED
/// (build 17, proto `UPDATE_MODES`). Table index 6 — a 1.0-era entry
/// present in every IddCx version, so unlike the *2 variants there is no
/// availability question at all.
///
/// Bound for symmetry with the query DDIs (both variants are registered),
/// but the driver CALLS ONLY [`monitor_update_modes2`]: the v1
/// `IDDCX_TARGET_MODE` has no `BitsPerComponent`, and our wire depth is
/// per-mode (`monitors::wire_bpc_for`), so a v1 push would silently drop
/// the 10/12-bit and HDR wire information `evt_query_target_modes2`
/// reports for the very same modes. Calling both would push the list
/// twice.
pub unsafe fn monitor_update_modes(
    monitor: IDDCX_MONITOR,
    in_args: *const IDARG_IN_UPDATEMODES,
) -> NTSTATUS {
    iddcx_call!(
        _IDDFUNCENUM_IddCxMonitorUpdateModesTableIndex as PFN_IDDCXMONITORUPDATEMODES,
        monitor,
        in_args
    )
}

/// v1.10 variant, table index 34 — THE entry build 17 calls. Carries
/// `IDDCX_TARGET_MODE2` (with `BitsPerComponent`), matching what
/// `evt_query_target_modes2` reports.
///
/// Safe to call unconditionally: IddMinimumVersionRequired = 10 means the
/// OS only loads us with a ≥1.10 function table (index 34 is inside the
/// 1.10 table of 36 entries). Note the C header wraps THIS entry — but
/// not index 6 — in `IDD_IS_FUNCTION_AVAILABLE`, whose else-branch calls
/// `WdfDriverErrorReportApiMissing` with `isFatal = TRUE`. Our macro
/// indexes the table directly and never reaches that branch, exactly like
/// `swapchain_release_and_acquire_buffer2` and
/// `monitor_query_hardware_cursor3`; the min-version declaration is what
/// makes that legal, so do not "fix" this into a guarded call.
pub unsafe fn monitor_update_modes2(
    monitor: IDDCX_MONITOR,
    in_args: *const IDARG_IN_UPDATEMODES2,
) -> NTSTATUS {
    iddcx_call!(
        _IDDFUNCENUM_IddCxMonitorUpdateModes2TableIndex as PFN_IDDCXMONITORUPDATEMODES2,
        monitor,
        in_args
    )
}

pub unsafe fn swapchain_set_device(
    swapchain: IDDCX_SWAPCHAIN,
    in_args: *const IDARG_IN_SWAPCHAINSETDEVICE,
) -> NTSTATUS {
    iddcx_call!(
        _IDDFUNCENUM_IddCxSwapChainSetDeviceTableIndex as PFN_IDDCXSWAPCHAINSETDEVICE,
        swapchain,
        in_args
    )
}

pub unsafe fn swapchain_release_and_acquire_buffer(
    swapchain: IDDCX_SWAPCHAIN,
    out_args: *mut IDARG_OUT_RELEASEANDACQUIREBUFFER,
) -> NTSTATUS {
    iddcx_call!(
        _IDDFUNCENUM_IddCxSwapChainReleaseAndAcquireBufferTableIndex as PFN_IDDCXSWAPCHAINRELEASEANDACQUIREBUFFER,
        swapchain,
        out_args
    )
}

/// v1.10 variant — mandatory for CAN_PROCESS_FP16 adapters (per-frame
/// IDDCX_METADATA2: HDR metadata, surface color space, SDR white level).
/// Safe to call unconditionally: IddMinimumVersionRequired = 10 means the
/// OS only loads us with a ≥1.10 function table.
pub unsafe fn swapchain_release_and_acquire_buffer2(
    swapchain: IDDCX_SWAPCHAIN,
    in_args: *mut IDARG_IN_RELEASEANDACQUIREBUFFER2,
    out_args: *mut IDARG_OUT_RELEASEANDACQUIREBUFFER2,
) -> NTSTATUS {
    iddcx_call!(
        _IDDFUNCENUM_IddCxSwapChainReleaseAndAcquireBuffer2TableIndex as PFN_IDDCXSWAPCHAINRELEASEANDACQUIREBUFFER2,
        swapchain,
        in_args,
        out_args
    )
}

pub unsafe fn swapchain_finished_processing_frame(swapchain: IDDCX_SWAPCHAIN) -> NTSTATUS {
    iddcx_call!(
        _IDDFUNCENUM_IddCxSwapChainFinishedProcessingFrameTableIndex as PFN_IDDCXSWAPCHAINFINISHEDPROCESSINGFRAME,
        swapchain
    )
}

pub unsafe fn monitor_setup_hardware_cursor(
    monitor: IDDCX_MONITOR,
    in_args: *const IDARG_IN_SETUP_HWCURSOR,
) -> NTSTATUS {
    iddcx_call!(
        _IDDFUNCENUM_IddCxMonitorSetupHardwareCursorTableIndex as PFN_IDDCXMONITORSETUPHARDWARECURSOR,
        monitor,
        in_args
    )
}

pub unsafe fn monitor_query_hardware_cursor(
    monitor: IDDCX_MONITOR,
    in_args: *const IDARG_IN_QUERY_HWCURSOR,
    out_args: *mut IDARG_OUT_QUERY_HWCURSOR,
) -> NTSTATUS {
    iddcx_call!(
        _IDDFUNCENUM_IddCxMonitorQueryHardwareCursorTableIndex as PFN_IDDCXMONITORQUERYHARDWARECURSOR,
        monitor,
        in_args,
        out_args
    )
}

/// v1.4 replacement adding PositionValid/PositionId. The FP16 (HDR)
/// adapter contract can reject older query variants with
/// STATUS_NOT_SUPPORTED — the cursor worker discovers the accepted
/// variant at runtime (3 → 2 → 1).
pub unsafe fn monitor_query_hardware_cursor2(
    monitor: IDDCX_MONITOR,
    in_args: *const IDARG_IN_QUERY_HWCURSOR,
    out_args: *mut IDARG_OUT_QUERY_HWCURSOR2,
) -> NTSTATUS {
    iddcx_call!(
        _IDDFUNCENUM_IddCxMonitorQueryHardwareCursor2TableIndex as PFN_IDDCXMONITORQUERYHARDWARECURSOR2,
        monitor,
        in_args,
        out_args
    )
}

/// v1.10 variant additionally carrying the cursor SdrWhiteLevel for HDR
/// compositing.
pub unsafe fn monitor_query_hardware_cursor3(
    monitor: IDDCX_MONITOR,
    in_args: *const IDARG_IN_QUERY_HWCURSOR,
    out_args: *mut IDARG_OUT_QUERY_HWCURSOR3,
) -> NTSTATUS {
    iddcx_call!(
        _IDDFUNCENUM_IddCxMonitorQueryHardwareCursor3TableIndex as PFN_IDDCXMONITORQUERYHARDWARECURSOR3,
        monitor,
        in_args,
        out_args
    )
}

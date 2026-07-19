// SPDX-License-Identifier: AGPL-3.0-only
//! Control plane: EvtIddCxDeviceIoControl → [`crate::dispatch::dispatch`],
//! effect application, per-handle contexts, and registry persistence.

use core::ptr::null_mut;

use wdk_sys::{
    call_unsafe_wdf_function_binding, NTSTATUS, PVOID, STATUS_DEVICE_NOT_READY,
    STATUS_INVALID_DEVICE_REQUEST, STATUS_INVALID_PARAMETER, STATUS_SUCCESS, ULONG,
    UNICODE_STRING, WDFDEVICE, WDFFILEOBJECT, WDFKEY, WDFREQUEST,
};

use super::PROVIDER;
use super::{monitors, Shell};
use crate::dispatch::{dispatch, Effect, HandleCtx, Status};
use luminal_driver_proto::ioctl;

/// Registry value under the device hardware key holding the persisted
/// state blob (identity reservations + permanent pool).
static VALUE_NAME: [u16; 16] = super::wide("LuminalVgdState");

fn value_name_unicode() -> UNICODE_STRING {
    let bytes = ((VALUE_NAME.len() - 1) * 2) as u16;
    UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes + 2,
        Buffer: VALUE_NAME.as_ptr().cast_mut(),
    }
}

const PLUGPLAY_REGKEY_DEVICE: ULONG = 1;
const KEY_READ_ACCESS: u32 = 0x2001_9; // KEY_READ
const KEY_WRITE_ACCESS: u32 = 0x2000_6; // KEY_WRITE
const REG_BINARY_TYPE: ULONG = 3;

unsafe fn open_device_key(device: WDFDEVICE, access: u32) -> Option<WDFKEY> {
    let mut key: WDFKEY = null_mut();
    let status = call_unsafe_wdf_function_binding!(
        WdfDeviceOpenRegistryKey,
        device,
        PLUGPLAY_REGKEY_DEVICE,
        access,
        wdk_sys::WDF_NO_OBJECT_ATTRIBUTES,
        &mut key
    );
    (status == STATUS_SUCCESS).then_some(key)
}

/// Read the persisted blob at device add (before the shell global
/// exists). Any failure is "no state" — parsing is defensive anyway.
pub unsafe fn read_persisted(device: WDFDEVICE) -> Option<Vec<u8>> {
    let key = open_device_key(device, KEY_READ_ACCESS)?;
    let name = value_name_unicode();
    let mut buf = vec![0u8; 4096];
    let mut got: ULONG = 0;
    let mut vtype: ULONG = 0;
    let status = call_unsafe_wdf_function_binding!(
        WdfRegistryQueryValue,
        key,
        &name,
        buf.len() as ULONG,
        buf.as_mut_ptr().cast::<core::ffi::c_void>(),
        &mut got,
        &mut vtype
    );
    call_unsafe_wdf_function_binding!(WdfRegistryClose, key);
    if status != STATUS_SUCCESS || vtype != REG_BINARY_TYPE || got == 0 {
        return None;
    }
    buf.truncate(got as usize);
    Some(buf)
}

unsafe fn write_persisted(blob: &[u8]) {
    let shell = Shell::get();
    let Some(device) = shell.wdf_device.get() else { return };
    let Some(key) = open_device_key(device.0.cast(), KEY_WRITE_ACCESS) else {
        return;
    };
    let name = value_name_unicode();
    let status = call_unsafe_wdf_function_binding!(
        WdfRegistryAssignValue,
        key,
        &name,
        REG_BINARY_TYPE,
        blob.len() as ULONG,
        blob.as_ptr().cast::<core::ffi::c_void>().cast_mut()
    );
    call_unsafe_wdf_function_binding!(WdfRegistryClose, key);
    if status != STATUS_SUCCESS {
        tracelogging::write_event!(
            PROVIDER,
            "PersistFailed",
            level(Error),
            i32("status", &status)
        );
    }
}

/// Apply dispatcher side effects. Called with NO locks held — plugging
/// and unplugging call into IddCx (DESIGN.md §3.3 rule 3).
pub fn apply_effects(effects: Vec<Effect>) {
    for effect in effects {
        match effect {
            Effect::PlugMonitor {
                session_id,
                display_id,
                connector_index,
                modes,
                adapter_luid,
                ring_slots,
                edid,
            } => monitors::plug(
                session_id,
                display_id,
                connector_index,
                modes,
                adapter_luid,
                ring_slots,
                edid,
            ),
            Effect::UnplugMonitor { session_id } => monitors::unplug(session_id),
            Effect::PersistState(blob) => unsafe { write_persisted(&blob) },
        }
    }
}

/// Session-mutating IOCTLs are refused until the IddCx adapter is up
/// (dispatch would accept them, but the plug effect could not be applied).
fn requires_adapter(code: u32) -> bool {
    matches!(
        code,
        ioctl::IOCTL_CREATE_MONITOR
            | ioctl::IOCTL_DESTROY_MONITOR
            | ioctl::IOCTL_SET_PERMANENT_POOL
    )
}

pub unsafe extern "C" fn evt_ioctl(
    _device: WDFDEVICE,
    request: WDFREQUEST,
    output_len: usize,
    input_len: usize,
    code: ULONG,
) {
    let shell = Shell::get();
    if requires_adapter(code) && !shell.ready() {
        call_unsafe_wdf_function_binding!(WdfRequestComplete, request, STATUS_DEVICE_NOT_READY);
        return;
    }

    let mut in_ptr: PVOID = null_mut();
    let mut in_got: usize = 0;
    if input_len > 0 {
        let status = call_unsafe_wdf_function_binding!(
            WdfRequestRetrieveInputBuffer,
            request,
            0,
            &mut in_ptr,
            &mut in_got
        );
        if status != STATUS_SUCCESS {
            call_unsafe_wdf_function_binding!(WdfRequestComplete, request, status);
            return;
        }
    }
    let mut out_ptr: PVOID = null_mut();
    let mut out_got: usize = 0;
    if output_len > 0 {
        let status = call_unsafe_wdf_function_binding!(
            WdfRequestRetrieveOutputBuffer,
            request,
            0,
            &mut out_ptr,
            &mut out_got
        );
        if status != STATUS_SUCCESS {
            call_unsafe_wdf_function_binding!(WdfRequestComplete, request, status);
            return;
        }
    }

    let input: &[u8] = if in_got > 0 {
        core::slice::from_raw_parts(in_ptr.cast::<u8>(), in_got)
    } else {
        &[]
    };
    let output: &mut [u8] = if out_got > 0 {
        core::slice::from_raw_parts_mut(out_ptr.cast::<u8>(), out_got)
    } else {
        &mut []
    };

    let file_key =
        call_unsafe_wdf_function_binding!(WdfRequestGetFileObject, request) as usize;

    let result = {
        let mut handles = shell.handles.lock().unwrap();
        let handle = handles.entry(file_key).or_insert_with(HandleCtx::default);
        let mut dev = shell.dev.lock().unwrap();
        dispatch(&mut dev, handle, shell.now_ms(), code, input, output)
    };

    let (status, info): (NTSTATUS, usize) = match result.status {
        Status::Ok => (STATUS_SUCCESS, result.bytes_written),
        Status::BadBuffer => (STATUS_INVALID_PARAMETER, 0),
        Status::UnknownCode => (STATUS_INVALID_DEVICE_REQUEST, 0),
    };
    call_unsafe_wdf_function_binding!(
        WdfRequestCompleteWithInformation,
        request,
        status,
        info as u64
    );

    // OS work strictly after request completion, with no locks held.
    apply_effects(result.effects);
}

pub unsafe extern "C" fn evt_file_create(
    _device: WDFDEVICE,
    request: WDFREQUEST,
    file_object: WDFFILEOBJECT,
) {
    if let Some(shell) = Shell::try_get() {
        shell
            .handles
            .lock()
            .unwrap()
            .insert(file_object as usize, HandleCtx::default());
    }
    call_unsafe_wdf_function_binding!(WdfRequestComplete, request, STATUS_SUCCESS);
}

pub unsafe extern "C" fn evt_file_close(file_object: WDFFILEOBJECT) {
    if let Some(shell) = Shell::try_get() {
        shell.handles.lock().unwrap().remove(&(file_object as usize));
    }
}

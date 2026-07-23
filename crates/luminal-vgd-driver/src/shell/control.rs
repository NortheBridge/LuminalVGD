// SPDX-License-Identifier: AGPL-3.0-only
//! Control plane: EvtIddCxDeviceIoControl → [`crate::dispatch::dispatch`],
//! effect application, per-handle contexts, and registry persistence.

use core::ptr::null_mut;
use core::sync::atomic::{AtomicBool, Ordering};

use wdk_sys::{
    call_unsafe_wdf_function_binding, NTSTATUS, PVOID, STATUS_DEVICE_NOT_READY,
    STATUS_INVALID_DEVICE_REQUEST, STATUS_INVALID_PARAMETER, STATUS_SUCCESS, ULONG,
    UNICODE_STRING, WDFDEVICE, WDFFILEOBJECT, WDFKEY, WDFREQUEST,
};
use windows::Win32::Security::{
    CheckTokenMembership, CreateWellKnownSid, WinBuiltinAdministratorsSid, WinLocalSystemSid,
    PSID, SECURITY_MAX_SID_SIZE, WELL_KNOWN_SID_TYPE,
};

use super::PROVIDER;
use super::{monitors, Shell};
use crate::dispatch::{dispatch, Effect, HandleCtx, Status};
use luminal_driver_proto::ioctl;

const STATUS_ACCESS_DENIED: NTSTATUS = 0xC000_0022u32 as NTSTATUS;

/// Reference string of the control device interface. Opens through the
/// enumerated interface symlink carry `\LuminalVGDControl` as the file
/// name; that name (and only that name) is subject to the DESIGN.md §6
/// SYSTEM+Administrators check below.
static CONTROL_REF: [u16; 18] = super::wide("LuminalVGDControl");

pub(crate) fn control_ref_unicode() -> UNICODE_STRING {
    let bytes = ((CONTROL_REF.len() - 1) * 2) as u16;
    UNICODE_STRING {
        Length: bytes,
        MaximumLength: bytes + 2,
        Buffer: CONTROL_REF.as_ptr().cast_mut(),
    }
}

/// Case-insensitive check whether `name` (a file-object name) addresses
/// the control interface: `\LuminalVGDControl` with or without the
/// leading backslash.
fn is_control_name(name: &[u16]) -> bool {
    let want = &CONTROL_REF[..CONTROL_REF.len() - 1]; // strip NUL
    let name = if name.first() == Some(&(b'\\' as u16)) { &name[1..] } else { name };
    name.len() == want.len()
        && name
            .iter()
            .zip(want.iter())
            .all(|(&a, &b)| char_fold(a) == char_fold(b))
}

fn char_fold(c: u16) -> u16 {
    match c {
        0x61..=0x7A => c - 0x20, // a-z → A-Z (the ref string is ASCII)
        _ => c,
    }
}

fn well_known_sid(kind: WELL_KNOWN_SID_TYPE, buf: &mut [u8; SECURITY_MAX_SID_SIZE as usize]) -> Option<PSID> {
    let mut len = buf.len() as u32;
    unsafe {
        CreateWellKnownSid(kind, None, Some(PSID(buf.as_mut_ptr().cast())), &mut len).ok()?;
    }
    Some(PSID(buf.as_mut_ptr().cast()))
}

/// Runs inside `WdfRequestImpersonate` with the caller's token on the
/// thread: SYSTEM or (elevated) BUILTIN\Administrators passes. With a
/// NULL token handle, `CheckTokenMembership` evaluates the calling
/// thread's impersonation token — a filtered (non-elevated) admin token
/// has the Administrators SID deny-only, so it correctly fails.
unsafe extern "C" fn evt_impersonate(_request: WDFREQUEST, context: PVOID) {
    let allowed = &*(context as *const AtomicBool);
    let mut sid_buf = [0u8; SECURITY_MAX_SID_SIZE as usize];
    for kind in [WinLocalSystemSid, WinBuiltinAdministratorsSid] {
        let Some(sid) = well_known_sid(kind, &mut sid_buf) else { continue };
        let mut member = windows::Win32::Foundation::FALSE;
        if CheckTokenMembership(None, sid, &mut member).is_ok() && member.as_bool() {
            allowed.store(true, Ordering::Release);
            return;
        }
    }
}

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

    // §6 control-surface ACL: only handles that passed the file-create
    // authorization (control reference string + SYSTEM/Administrators
    // token) may issue IOCTLs — a handle opened any other way (e.g. the
    // bare device object without the reference string) is refused before
    // dispatch ever sees it. Missing context = deny.
    let file_key =
        call_unsafe_wdf_function_binding!(WdfRequestGetFileObject, request) as usize;
    let authorized = shell
        .handles
        .lock()
        .unwrap()
        .get(&file_key)
        .map(|h| h.authorized)
        .unwrap_or(false);
    if !authorized {
        call_unsafe_wdf_function_binding!(WdfRequestComplete, request, STATUS_ACCESS_DENIED);
        return;
    }

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
    // Which name is this open using? Interface opens through our control
    // symlink carry the reference string; the OS graphics stack's opens
    // of the same device object carry other (usually empty) names and
    // must pass unhindered (phase-2 lesson: they run unelevated).
    let name_ptr = call_unsafe_wdf_function_binding!(WdfFileObjectGetFileName, file_object);
    let name: &[u16] = if name_ptr.is_null() {
        &[]
    } else {
        let n = &*name_ptr;
        if n.Buffer.is_null() {
            &[]
        } else {
            core::slice::from_raw_parts(n.Buffer, (n.Length / 2) as usize)
        }
    };

    if !is_control_name(name) {
        // Not the control plane: allow, unauthorized (IOCTLs are refused).
        if let Some(shell) = Shell::try_get() {
            shell
                .handles
                .lock()
                .unwrap()
                .insert(file_object as usize, HandleCtx::default());
        }
        call_unsafe_wdf_function_binding!(WdfRequestComplete, request, STATUS_SUCCESS);
        return;
    }

    // Control-plane open: DESIGN.md §6 — SYSTEM or elevated Administrators
    // only. Evaluated under impersonation of the caller's token; any
    // failure (impersonation refused, token check failed) is a deny.
    let allowed = AtomicBool::new(false);
    let status = call_unsafe_wdf_function_binding!(
        WdfRequestImpersonate,
        request,
        wdk_sys::_SECURITY_IMPERSONATION_LEVEL::SecurityIdentification,
        Some(evt_impersonate),
        (&allowed as *const AtomicBool) as PVOID
    );
    let allowed = status == STATUS_SUCCESS && allowed.load(Ordering::Acquire);

    if !allowed {
        tracelogging::write_event!(PROVIDER, "ControlOpenDenied", level(Warning));
        call_unsafe_wdf_function_binding!(WdfRequestComplete, request, STATUS_ACCESS_DENIED);
        return;
    }
    if let Some(shell) = Shell::try_get() {
        shell.handles.lock().unwrap().insert(
            file_object as usize,
            HandleCtx { authorized: true, ..HandleCtx::default() },
        );
    }
    call_unsafe_wdf_function_binding!(WdfRequestComplete, request, STATUS_SUCCESS);
}

pub unsafe extern "C" fn evt_file_close(file_object: WDFFILEOBJECT) {
    if let Some(shell) = Shell::try_get() {
        shell.handles.lock().unwrap().remove(&(file_object as usize));
    }
}

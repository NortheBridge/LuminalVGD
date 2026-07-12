// SPDX-License-Identifier: AGPL-3.0-only
//! Control-device I/O (Windows only): enumerate the LuminalVGD interface,
//! issue the proto IOCTLs, and map the shared ring section read-only.
//!
//! Probe order per DESIGN.md §5: enumerate interface GUID → open →
//! handshake → CREATE_MONITOR → map ring. Every failure is returned, never
//! panicked — the caller converts it into a `ProbeOutcome::Unavailable`
//! and the controller falls through to WGC.

use std::ffi::c_void;
use std::io;
use std::mem::{size_of, MaybeUninit};
use std::ptr::{null, null_mut, read_volatile};

use luminal_driver_proto::{
    ioctl, names, ring_section_size, CreateMonitorReply, CreateMonitorRequest,
    DestroyMonitorRequest, GetStatusReply, HandshakeReply, HandshakeRequest,
    PermanentPoolConfig, PingRequest, QueryLeaseReply, QueryLeaseRequest,
    QueryPermanentPoolReply, RingHeader, SetRenderAdapterRequest, SlotMetadata,
    LUMINAL_VGD_INTERFACE_GUID, PROTO_VERSION_MAJOR, PROTO_VERSION_MINOR, RING_SLOTS_OFFSET,
};
use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Device_Interface_ListW, CM_Get_Device_Interface_List_SizeW,
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CR_SUCCESS,
};
use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_READ,
    MEMORY_MAPPED_VIEW_ADDRESS,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

const INVALID_HANDLE_VALUE: HANDLE = -1isize as HANDLE;

fn guid() -> GUID {
    let (d1, d2, d3, d4) = LUMINAL_VGD_INTERFACE_GUID;
    GUID { data1: d1, data2: d2, data3: d3, data4: d4 }
}

/// Open handle to the LuminalVGD control device.
pub struct VgdDevice {
    handle: HANDLE,
}

// The handle is only used through &self with kernel-synchronized IOCTLs.
unsafe impl Send for VgdDevice {}
unsafe impl Sync for VgdDevice {}

impl Drop for VgdDevice {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
    }
}

impl VgdDevice {
    /// Enumerate present LuminalVGD interfaces and open the first one.
    /// `Err(NotFound)` is the normal "driver absent" outcome.
    pub fn open_first() -> io::Result<Self> {
        let guid = guid();
        let mut len: u32 = 0;
        let cr = unsafe {
            CM_Get_Device_Interface_List_SizeW(
                &mut len,
                &guid,
                null(),
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        };
        if cr != CR_SUCCESS || len < 2 {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no LuminalVGD interface"));
        }
        let mut buf = vec![0u16; len as usize];
        let cr = unsafe {
            CM_Get_Device_Interface_ListW(
                &guid,
                null(),
                buf.as_mut_ptr(),
                len,
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        };
        if cr != CR_SUCCESS {
            return Err(io::Error::new(io::ErrorKind::NotFound, "interface list failed"));
        }
        // Multi-SZ: first NUL-terminated string is our device path.
        let first_len = buf.iter().position(|&c| c == 0).unwrap_or(0);
        if first_len == 0 {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no LuminalVGD interface"));
        }
        let handle = unsafe {
            CreateFileW(
                buf.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { handle })
    }

    /// METHOD_BUFFERED IOCTL with typed in/out. The driver must fill the
    /// full reply; a short reply is a protocol violation reported as an
    /// error, never a partially-initialized struct.
    fn ioctl_inout<I: Copy, O: Copy>(&self, code: u32, input: &I) -> io::Result<O> {
        let mut out = MaybeUninit::<O>::uninit();
        let mut returned: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                self.handle,
                code,
                (input as *const I).cast::<c_void>(),
                size_of::<I>() as u32,
                out.as_mut_ptr().cast::<c_void>(),
                size_of::<O>() as u32,
                &mut returned,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if returned as usize != size_of::<O>() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("short IOCTL reply: {returned} of {} bytes", size_of::<O>()),
            ));
        }
        Ok(unsafe { out.assume_init() })
    }

    pub fn handshake(&self) -> io::Result<HandshakeReply> {
        let req = HandshakeRequest {
            host_proto_major: PROTO_VERSION_MAJOR,
            host_proto_minor: PROTO_VERSION_MINOR,
        };
        self.ioctl_inout(ioctl::IOCTL_HANDSHAKE, &req)
    }

    pub fn create_monitor(&self, req: &CreateMonitorRequest) -> io::Result<CreateMonitorReply> {
        self.ioctl_inout(ioctl::IOCTL_CREATE_MONITOR, req)
    }

    /// Returns the proto result code (`err::OK` or negative).
    pub fn destroy_monitor(&self, session_id: u64) -> io::Result<i32> {
        self.ioctl_inout(ioctl::IOCTL_DESTROY_MONITOR, &DestroyMonitorRequest { session_id })
    }

    /// Returns the proto result code. Call at least once per watchdog
    /// period (the handshake reports the effective period).
    pub fn ping(&self, session_id: u64) -> io::Result<i32> {
        self.ioctl_inout(ioctl::IOCTL_PING, &PingRequest { session_id })
    }

    pub fn get_status(&self) -> io::Result<GetStatusReply> {
        // No input payload.
        self.ioctl_inout(ioctl::IOCTL_GET_STATUS, &())
    }

    /// Lease introspection (proto v0.3).
    pub fn query_lease(&self, session_id: u64) -> io::Result<QueryLeaseReply> {
        self.ioctl_inout(ioctl::IOCTL_QUERY_LEASE, &QueryLeaseRequest { session_id })
    }

    /// Set the device-wide preferred render adapter (0 clears).
    pub fn set_render_adapter(&self, adapter_luid: u64) -> io::Result<i32> {
        self.ioctl_inout(ioctl::IOCTL_SET_RENDER_ADAPTER, &SetRenderAdapterRequest { adapter_luid })
    }

    /// Configure the permanent display pool (count 0 disbands).
    pub fn set_permanent_pool(&self, config: &PermanentPoolConfig) -> io::Result<i32> {
        self.ioctl_inout(ioctl::IOCTL_SET_PERMANENT_POOL, config)
    }

    pub fn query_permanent_pool(&self) -> io::Result<QueryPermanentPoolReply> {
        self.ioctl_inout(ioctl::IOCTL_QUERY_PERMANENT_POOL, &())
    }
}

/// Read-only mapping of a monitor's shared ring section.
pub struct RingView {
    mapping: HANDLE,
    view: *const u8,
    slot_count: u32,
}

unsafe impl Send for RingView {}

impl Drop for RingView {
    fn drop(&mut self) {
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.view.cast_mut().cast(),
            });
            CloseHandle(self.mapping);
        }
    }
}

impl RingView {
    /// Map the ring section for `session_id` (name derived through the
    /// shared proto helper — never hand-composed).
    pub fn open(session_id: u64, slot_count: u32) -> io::Result<Self> {
        let mut name = [0u16; 64];
        names::ring_section_name(session_id, &mut name);
        let mapping = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, name.as_ptr()) };
        if mapping.is_null() {
            return Err(io::Error::last_os_error());
        }
        let view = unsafe {
            MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, ring_section_size(slot_count))
        };
        if view.Value.is_null() {
            let e = io::Error::last_os_error();
            unsafe { CloseHandle(mapping) };
            return Err(e);
        }
        Ok(Self { mapping, view: view.Value.cast::<u8>().cast_const(), slot_count })
    }

    /// Volatile snapshot of the header (driver writes concurrently).
    pub fn header(&self) -> RingHeader {
        unsafe { read_volatile(self.view.cast::<RingHeader>()) }
    }

    /// Volatile snapshot of one slot's metadata.
    pub fn slot(&self, index: u32) -> Option<SlotMetadata> {
        if index >= self.slot_count {
            return None;
        }
        let offset = RING_SLOTS_OFFSET + index as usize * size_of::<SlotMetadata>();
        Some(unsafe { read_volatile(self.view.add(offset).cast::<SlotMetadata>()) })
    }
}

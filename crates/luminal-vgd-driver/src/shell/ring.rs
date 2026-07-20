// SPDX-License-Identifier: AGPL-3.0-only
//! The shared frame ring, driver side (DESIGN.md §3.1).
//!
//! Owns the OS objects the transport is made of: the named shared-memory
//! control section (`RingHeader` + `SlotMetadata[]`, layout from
//! luminal-driver-proto — the host maps the same bytes read-only) and the
//! named keyed-mutex shared textures the frames travel in. All slot
//! *decisions* belong to `core::ring::RingPolicy`; this module only
//! executes them against D3D/Win32.
//!
//! Object security: SYSTEM + Administrators (LuminalShine's service is
//! SYSTEM; dev tooling runs elevated). The generation is baked into
//! texture names by the proto helpers, so a rebuilt ring can never alias
//! stale handles.

use core::ffi::c_void;
use core::mem::size_of;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, LocalFree, HLOCAL};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX, D3D11_RESOURCE_MISC_SHARED_NTHANDLE,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    IDXGIKeyedMutex, IDXGIResource1, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE,
};
use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS, PAGE_READWRITE,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

use luminal_driver_proto::{
    names, ring_section_size, ring_state, Hdr10StaticMetadata, RectU32, RingHeader, SlotMetadata,
    RING_HEADER_VERSION, RING_MAGIC, RING_SLOTS_OFFSET,
};

/// SYSTEM + Administrators, full access (matches DESIGN.md §6 intent for
/// the data plane; the encoder service is SYSTEM).
const RING_SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)";

pub fn qpc_now() -> u64 {
    let mut v = 0i64;
    let _ = unsafe { QueryPerformanceCounter(&mut v) };
    v as u64
}

pub fn qpc_frequency() -> u64 {
    let mut v = 0i64;
    let _ = unsafe { QueryPerformanceFrequency(&mut v) };
    v as u64
}

/// Run `f` with a SECURITY_ATTRIBUTES for [`RING_SDDL`]. The descriptor
/// is LocalFree'd afterwards, so it must not outlive the call.
fn with_ring_security<T>(
    f: impl FnOnce(*const SECURITY_ATTRIBUTES) -> windows::core::Result<T>,
) -> windows::core::Result<T> {
    let sddl: Vec<u16> = RING_SDDL.encode_utf16().chain(Some(0)).collect();
    let mut sd = PSECURITY_DESCRIPTOR(null_mut());
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            1, // SDDL_REVISION_1
            &mut sd,
            None,
        )?;
    }
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: false.into(),
    };
    let result = f(&sa);
    unsafe { LocalFree(Some(HLOCAL(sd.0))) };
    result
}

/// Writable mapping of the ring control section. One per live monitor,
/// owned by its frame worker.
pub struct RingSection {
    mapping: HANDLE,
    view: *mut u8,
    slot_count: u32,
}

// SAFETY: the view is only written through volatile/atomic operations and
// the struct is owned by a single worker thread at a time.
unsafe impl Send for RingSection {}

impl Drop for RingSection {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(windows::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.view.cast::<c_void>(),
            });
            let _ = CloseHandle(self.mapping);
        }
    }
}

impl RingSection {
    /// Create + map `Global\LuminalVGD-ring-<session>` and initialize the
    /// header (state ACTIVE, generation 1, fresh heartbeat).
    pub fn create(session_id: u64, slot_count: u32) -> windows::core::Result<Self> {
        let mut name = [0u16; 64];
        names::ring_section_name(session_id, &mut name);
        let size = ring_section_size(slot_count);

        let mapping = with_ring_security(|sa| unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                Some(sa),
                PAGE_READWRITE,
                0,
                size as u32,
                PCWSTR(name.as_ptr()),
            )
        })?;

        let view = unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, size) };
        if view.Value.is_null() {
            let err = windows::core::Error::from_win32();
            unsafe {
                let _ = CloseHandle(mapping);
            }
            return Err(err);
        }
        let ring = Self {
            mapping,
            view: view.Value.cast::<u8>(),
            slot_count,
        };

        unsafe {
            core::ptr::write_bytes(ring.view, 0, size);
            let h = ring.header_mut();
            core::ptr::write_volatile(&mut (*h).header_version, RING_HEADER_VERSION);
            core::ptr::write_volatile(&mut (*h).ring_generation, 1);
            core::ptr::write_volatile(&mut (*h).slot_count, slot_count);
            core::ptr::write_volatile(&mut (*h).qpc_frequency, qpc_frequency());
            core::ptr::write_volatile(&mut (*h).driver_heartbeat_qpc, qpc_now());
            // State + magic last: an all-zero or magic-less section is the
            // documented "not initialized yet" signal for the host.
            ring.state_atomic().store(ring_state::ACTIVE, Ordering::Release);
            core::ptr::write_volatile(&mut (*h).magic, RING_MAGIC);
        }
        Ok(ring)
    }

    fn header_mut(&self) -> *mut RingHeader {
        self.view.cast::<RingHeader>()
    }

    fn state_atomic(&self) -> &AtomicU32 {
        unsafe { AtomicU32::from_ptr(&mut (*self.header_mut()).state) }
    }

    fn slot_ptr(&self, index: usize) -> *mut SlotMetadata {
        debug_assert!(index < self.slot_count as usize);
        unsafe {
            self.view
                .add(RING_SLOTS_OFFSET + index * size_of::<SlotMetadata>())
                .cast::<SlotMetadata>()
        }
    }

    pub fn set_state(&self, state: u32) {
        self.state_atomic().store(state, Ordering::Release);
    }

    pub fn set_generation(&self, generation: u32) {
        unsafe {
            core::ptr::write_volatile(&mut (*self.header_mut()).ring_generation, generation);
        }
    }

    pub fn heartbeat(&self) {
        unsafe {
            core::ptr::write_volatile(&mut (*self.header_mut()).driver_heartbeat_qpc, qpc_now());
        }
    }

    /// Mark a slot as being written (state WRITING, release-ordered).
    pub fn slot_writing(&self, index: usize) {
        let slot = self.slot_ptr(index);
        unsafe {
            AtomicU32::from_ptr(&mut (*slot).state)
                .store(luminal_driver_proto::slot_state::WRITING, Ordering::Release);
        }
    }

    /// Publish a completed slot: metadata first, state PUBLISHED last
    /// (release), then the header counters.
    pub fn slot_published(
        &self,
        index: usize,
        sequence: u64,
        present_qpc: u64,
        frames_published: u64,
        frames_dropped: u64,
    ) {
        let slot = self.slot_ptr(index);
        unsafe {
            core::ptr::write_volatile(&mut (*slot).sequence, sequence);
            core::ptr::write_volatile(&mut (*slot).present_qpc, present_qpc);
            core::ptr::write_volatile(&mut (*slot).flags, 0);
            core::ptr::write_volatile(&mut (*slot).hdr, Hdr10StaticMetadata::default());
            core::ptr::write_volatile(&mut (*slot).dirty_count, 0);
            core::ptr::write_volatile(&mut (*slot).dirty_bound, RectU32::default());
            AtomicU32::from_ptr(&mut (*slot).state)
                .store(luminal_driver_proto::slot_state::PUBLISHED, Ordering::Release);

            let h = self.header_mut();
            AtomicU64::from_ptr(&mut (*h).latest_sequence).store(sequence, Ordering::Release);
            core::ptr::write_volatile(&mut (*h).latest_present_qpc, present_qpc);
            core::ptr::write_volatile(&mut (*h).frames_published, frames_published);
            core::ptr::write_volatile(&mut (*h).frames_dropped, frames_dropped);
            core::ptr::write_volatile(&mut (*h).driver_heartbeat_qpc, qpc_now());
        }
    }

    /// Acquire-load one slot's shared state (the host writes READING/FREE
    /// transitions here — feed into `RingPolicy::reconcile_shared`).
    pub fn slot_state(&self, index: usize) -> u32 {
        let slot = self.slot_ptr(index);
        unsafe { AtomicU32::from_ptr(&mut (*slot).state).load(Ordering::Acquire) }
    }

    /// Reset one slot to FREE (aborted write).
    pub fn reset_slot_free(&self, index: usize) {
        let slot = self.slot_ptr(index);
        unsafe {
            AtomicU32::from_ptr(&mut (*slot).state)
                .store(luminal_driver_proto::slot_state::FREE, Ordering::Release);
        }
    }

    /// Reset all slots to FREE (rebuild path) without touching sequences.
    pub fn reset_slots(&self) {
        for i in 0..self.slot_count as usize {
            let slot = self.slot_ptr(i);
            unsafe {
                AtomicU32::from_ptr(&mut (*slot).state)
                    .store(luminal_driver_proto::slot_state::FREE, Ordering::Release);
            }
        }
    }
}

/// One named keyed-mutex shared texture (one ring slot's pixel storage).
pub struct SharedTexture {
    pub texture: ID3D11Texture2D,
    pub mutex: IDXGIKeyedMutex,
    /// The named NT handle keeps the name alive for the session.
    name_handle: HANDLE,
}

// SAFETY: COM pointers used from the single worker thread; the handle is
// only closed on drop.
unsafe impl Send for SharedTexture {}

impl Drop for SharedTexture {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.name_handle);
        }
    }
}

/// Keyed-mutex acquire with a raw HRESULT so WAIT_TIMEOUT (a success-class
/// HRESULT the windows crate would fold into Ok) stays distinguishable —
/// the §3.3 bounded-wait contract depends on seeing timeouts.
pub enum AcquireOutcome {
    Acquired,
    TimedOut,
    DeviceLost(windows::core::HRESULT),
}

pub fn acquire_mutex(mutex: &IDXGIKeyedMutex, key: u64, timeout_ms: u32) -> AcquireOutcome {
    const WAIT_TIMEOUT_HR: i32 = 0x0000_0102; // HRESULT_FROM_WIN32-free: returned as-is
    let hr = unsafe {
        (Interface::vtable(mutex).AcquireSync)(Interface::as_raw(mutex), key, timeout_ms)
    };
    if hr.0 == 0 {
        AcquireOutcome::Acquired
    } else if hr.0 == WAIT_TIMEOUT_HR {
        AcquireOutcome::TimedOut
    } else {
        AcquireOutcome::DeviceLost(hr)
    }
}

/// Create the `count` named shared textures for (`session_id`,
/// `generation`) at the given dimensions and format. The format follows
/// the acquired swapchain frame's desc — BGRA8 for SDR desktops, FP16
/// (scRGB) or R10G10B10A2 once the OS composes the display in advanced
/// color; keyed mutex starts at key 0 (driver-writable).
pub fn create_shared_textures(
    device: &ID3D11Device,
    session_id: u64,
    generation: u32,
    count: u32,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
) -> windows::core::Result<Vec<SharedTexture>> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: (D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0
            | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0) as u32,
    };

    let mut out = Vec::with_capacity(count as usize);
    for slot in 0..count {
        let mut texture: Option<ID3D11Texture2D> = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture))? };
        let texture = texture.expect("CreateTexture2D succeeded without a texture");

        let mut name = [0u16; 96];
        names::slot_texture_name(session_id, generation, slot, &mut name);
        let resource: IDXGIResource1 = texture.cast()?;
        let name_handle = with_ring_security(|sa| unsafe {
            resource.CreateSharedHandle(
                Some(sa),
                DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                PCWSTR(name.as_ptr()),
            )
        })?;
        let mutex: IDXGIKeyedMutex = texture.cast()?;
        out.push(SharedTexture { texture, mutex, name_handle });
    }
    Ok(out)
}

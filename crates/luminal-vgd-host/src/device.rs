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
    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_READ, FILE_MAP_WRITE,
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

/// Mapping of a monitor's shared ring section. Reads are volatile
/// snapshots; the ONLY writes the host ever performs are the reader-side
/// slot-state transitions (`PUBLISHED→READING→FREE`, atomic CAS) — the
/// rest of the section belongs to the driver.
pub struct RingView {
    mapping: HANDLE,
    view: *mut u8,
    slot_count: u32,
}

unsafe impl Send for RingView {}

impl Drop for RingView {
    fn drop(&mut self) {
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.view.cast(),
            });
            CloseHandle(self.mapping);
        }
    }
}

/// A ring slot the host has checked out (shared state `READING`): the
/// driver will not touch its texture until [`RingView::release`]. Copy of
/// the claim-time metadata; the texture itself is opened by name on the
/// consumer's D3D device (`names::slot_texture_name` with `generation`).
#[derive(Clone, Copy, Debug)]
pub struct ClaimedFrame {
    pub index: u32,
    pub sequence: u64,
    pub present_qpc: u64,
    /// Ring generation at claim time — bake into the texture name; if the
    /// header generation no longer matches at use time, release and
    /// re-claim (the driver rebuilt the ring).
    pub generation: u32,
}

impl RingView {
    /// Test-only: a view over caller-owned memory instead of a mapped
    /// section. The caller must keep the buffer alive and `mem::forget`
    /// the view (Drop would try to unmap it).
    #[cfg(test)]
    pub(crate) fn over_buffer(view: *mut u8, slot_count: u32) -> Self {
        Self { mapping: core::ptr::null_mut(), view, slot_count }
    }

    /// Map the ring section for `session_id` (name derived through the
    /// shared proto helper — never hand-composed).
    pub fn open(session_id: u64, slot_count: u32) -> io::Result<Self> {
        let mut name = [0u16; 64];
        names::ring_section_name(session_id, &mut name);
        let access = FILE_MAP_READ | FILE_MAP_WRITE;
        let mapping = unsafe { OpenFileMappingW(access, 0, name.as_ptr()) };
        if mapping.is_null() {
            return Err(io::Error::last_os_error());
        }
        let view =
            unsafe { MapViewOfFile(mapping, access, 0, 0, ring_section_size(slot_count)) };
        if view.Value.is_null() {
            let e = io::Error::last_os_error();
            unsafe { CloseHandle(mapping) };
            return Err(e);
        }
        Ok(Self { mapping, view: view.Value.cast::<u8>(), slot_count })
    }

    /// Volatile snapshot of the header (driver writes concurrently).
    pub fn header(&self) -> RingHeader {
        unsafe { read_volatile(self.view.cast::<RingHeader>().cast_const()) }
    }

    fn slot_ptr(&self, index: u32) -> *mut SlotMetadata {
        debug_assert!(index < self.slot_count);
        unsafe {
            self.view
                .add(RING_SLOTS_OFFSET + index as usize * size_of::<SlotMetadata>())
                .cast::<SlotMetadata>()
        }
    }

    fn slot_state_atomic(&self, index: u32) -> &core::sync::atomic::AtomicU32 {
        unsafe { core::sync::atomic::AtomicU32::from_ptr(&mut (*self.slot_ptr(index)).state) }
    }

    /// Volatile snapshot of one slot's metadata.
    pub fn slot(&self, index: u32) -> Option<SlotMetadata> {
        if index >= self.slot_count {
            return None;
        }
        Some(unsafe { read_volatile(self.slot_ptr(index).cast_const()) })
    }

    /// Check out the freshest published frame: pick the PUBLISHED slot
    /// with the highest sequence and CAS it `PUBLISHED→READING`. Returns
    /// None when nothing is published (or the driver won every race).
    /// Streams want freshness — older published frames stay eligible for
    /// the driver's drop-oldest overwrite.
    pub fn claim_latest(&self) -> Option<ClaimedFrame> {
        use core::sync::atomic::Ordering;
        use luminal_driver_proto::slot_state as ss;
        // Bounded retries: a failed CAS means the driver took the slot
        // mid-claim; rescan at most once per slot.
        for _ in 0..=self.slot_count {
            let mut newest: Option<ClaimedFrame> = None;
            for index in 0..self.slot_count {
                let meta = self.slot(index)?;
                if meta.state == ss::PUBLISHED
                    && newest.is_none_or(|n| meta.sequence > n.sequence)
                {
                    newest = Some(ClaimedFrame {
                        index,
                        sequence: meta.sequence,
                        present_qpc: meta.present_qpc,
                        generation: self.header().ring_generation,
                    });
                }
            }
            let candidate = newest?;
            if self
                .slot_state_atomic(candidate.index)
                .compare_exchange(ss::PUBLISHED, ss::READING, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                // Re-read the metadata now that READING protects the slot.
                // The scan above races the driver's drop-oldest republish:
                // the state can be PUBLISHED again (ABA) with a NEWER
                // sequence by CAS time, and returning the stale pre-CAS
                // sequence mislabels the claim — a consumer deduping by
                // sequence then discards a frame that (if the desktop goes
                // idle) can never be re-claimed. The CAS's acquire ordering
                // pairs with the driver's release publish, so these reads
                // see the full metadata of whatever frame the slot holds.
                let meta = self.slot(candidate.index)?;
                return Some(ClaimedFrame {
                    index: candidate.index,
                    sequence: meta.sequence,
                    present_qpc: meta.present_qpc,
                    generation: self.header().ring_generation,
                });
            }
        }
        None
    }

    /// Release a claimed slot back to the driver (`READING→FREE`). Safe
    /// to call exactly once per successful claim.
    pub fn release(&self, index: u32) {
        use core::sync::atomic::Ordering;
        use luminal_driver_proto::slot_state as ss;
        let _ = self.slot_state_atomic(index).compare_exchange(
            ss::READING,
            ss::FREE,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
    use luminal_driver_proto::slot_state as ss;
    use luminal_vgd_core::ring::RingPolicy;

    /// Regression for the ring-stall bug (2026-07-22): `claim_latest`'s
    /// pre-CAS metadata scan races the driver's drop-oldest republish. The
    /// CAS can succeed against a slot that was republished with a NEWER
    /// sequence in between (ABA on the state value), and returning the
    /// stale scanned sequence mislabels the claim. A consumer deduping by
    /// sequence then discards the ring's freshest frame; if the desktop
    /// idles, that frame is never re-claimable — the observed live stall
    /// (latest_sequence frozen above the last delivered sequence).
    ///
    /// The writer thread below mimics the driver's exact shared-memory
    /// protocol (RingPolicy decisions + metadata-then-state-Release
    /// publishes); the reader claims/releases with the capture backend's
    /// dedupe. The invariant that catches the bug: a claim's reported
    /// sequence must always equal the sequence the slot holds while it is
    /// READING-protected.
    #[test]
    fn claim_sequence_is_coherent_under_republish_races() {
        const SLOTS: u32 = 3;
        const FRAMES: u64 = 200_000;

        let bytes = ring_section_size(SLOTS);
        let mut buf: Vec<u64> = vec![0; bytes.div_ceil(8)];
        let base_addr = buf.as_mut_ptr() as usize;

        let stop = AtomicBool::new(false);
        let published_latest = AtomicU64::new(0);

        std::thread::scope(|s| {
            // Driver-side writer: reconcile → writer_acquire → metadata
            // writes → state PUBLISHED (Release) → header latest_sequence.
            let stop_ref = &stop;
            let published_ref = &published_latest;
            let w = s.spawn(move || {
                let v = RingView::over_buffer(base_addr as *mut u8, SLOTS);
                let mut policy = RingPolicy::new(SLOTS);
                let mut published = 0u64;
                while published < FRAMES && !stop_ref.load(Ordering::Acquire) {
                    for i in 0..SLOTS {
                        let state = unsafe {
                            AtomicU32::from_ptr(&mut (*v.slot_ptr(i)).state)
                                .load(Ordering::Acquire)
                        };
                        policy.reconcile_shared(i as usize, state);
                    }
                    let Some(wslot) = policy.writer_acquire() else {
                        std::hint::spin_loop();
                        continue;
                    };
                    // The driver's take-CAS: serialize against the host's
                    // claim CAS on the same atomic; on loss, drop the frame
                    // (next reconcile absorbs the host's READING).
                    let expected = if wslot.overwrote.is_some() {
                        ss::PUBLISHED
                    } else {
                        ss::FREE
                    };
                    let taken = unsafe {
                        AtomicU32::from_ptr(&mut (*v.slot_ptr(wslot.index as u32)).state)
                            .compare_exchange(
                                expected,
                                ss::WRITING,
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            )
                            .is_ok()
                    };
                    if !taken {
                        policy.writer_abort(wslot.index);
                        continue;
                    }
                    let seq = policy.publish(wslot.index);
                    unsafe {
                        let slot = v.slot_ptr(wslot.index as u32);
                        core::ptr::write_volatile(&mut (*slot).sequence, seq);
                        core::ptr::write_volatile(&mut (*slot).present_qpc, seq);
                        AtomicU32::from_ptr(&mut (*slot).state)
                            .store(ss::PUBLISHED, Ordering::Release);
                    }
                    published_ref.store(seq, Ordering::Release);
                    published = seq;
                }
                core::mem::forget(v);
            });

            // Host-side reader: claim → verify coherence → dedupe → release.
            let r = s.spawn(move || {
                let v = RingView::over_buffer(base_addr as *mut u8, SLOTS);
                let mut last_delivered = 0u64;
                let mut delivered = 0u64;
                while published_ref.load(Ordering::Acquire) < FRAMES {
                    let Some(claim) = v.claim_latest() else {
                        std::hint::spin_loop();
                        continue;
                    };
                    // THE regression assertion: while READING-protected,
                    // the slot's actual sequence must match the claim.
                    let held = v.slot(claim.index).unwrap();
                    assert_eq!(
                        held.sequence, claim.sequence,
                        "claim mislabeled: slot holds {} but claim says {}",
                        held.sequence, claim.sequence
                    );
                    // Consumer contract: deliver only frames NEWER than the
                    // last delivered one. Older still-PUBLISHED leftovers
                    // are legitimately claimable after the newest slot is
                    // released — a `!=` dedupe would deliver them out of
                    // order (the second defect this test exposed).
                    if claim.sequence > last_delivered {
                        last_delivered = claim.sequence;
                        delivered += 1;
                    }
                    v.release(claim.index);
                }
                core::mem::forget(v);
                (last_delivered, delivered)
            });

            w.join().unwrap();
            let (last_delivered, delivered) = r.join().unwrap();
            stop.store(true, Ordering::Release);

            // End-state invariant (the live failure mode): once the writer
            // idles, the freshest published frame must still be claimable.
            let v = RingView::over_buffer(base_addr as *mut u8, SLOTS);
            let final_latest = published_latest.load(Ordering::Acquire);
            if last_delivered < final_latest {
                let claim = v
                    .claim_latest()
                    .expect("freshest frame must remain claimable after writer idles");
                assert_eq!(claim.sequence, final_latest);
                v.release(claim.index);
            }
            core::mem::forget(v);
            assert!(delivered > 0, "reader must have consumed frames");
        });
    }
}

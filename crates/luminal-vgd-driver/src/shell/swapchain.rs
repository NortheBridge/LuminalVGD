// SPDX-License-Identifier: AGPL-3.0-only
//! Swap-chain assignment and the per-monitor frame worker.
//!
//! Phase-2 scope: accept the swap chain, bind a D3D device on the LUID
//! the OS chose, and drain frames (acquire → finished) so the OS frame
//! watchdog stays satisfied. The shared-texture ring lands in phase 4;
//! `core::ring::RingPolicy` will own all slot decisions there.
//!
//! §3.3 discipline: the IddCx callbacks only start/stop the worker; every
//! worker wait is bounded (100 ms), so stop → join is bounded too.

use core::mem::zeroed;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use wdk_sys::{NTSTATUS, STATUS_PENDING, STATUS_SUCCESS};
use windows::core::Interface;
use windows::Win32::Foundation::{HANDLE, HMODULE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIDevice, IDXGIFactory4,
};
use windows::Win32::System::Threading::WaitForSingleObject;

use super::bindings::{self, ffi};
use super::{OsHandle, Shell, PROVIDER};

pub(crate) struct Worker {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Worker {
    /// Bounded stop: the thread re-checks the flag at least every 100 ms.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

pub unsafe extern "C" fn evt_assign(
    monitor: ffi::IDDCX_MONITOR,
    in_args: *const ffi::IDARG_IN_SETSWAPCHAIN,
) -> NTSTATUS {
    let inp = &*in_args;
    let swapchain = OsHandle(inp.hSwapChain.cast());
    let frame_event = OsHandle(inp.hNextSurfaceAvailable.cast());
    let luid = ((inp.RenderAdapterLuid.HighPart as u32 as u64) << 32)
        | inp.RenderAdapterLuid.LowPart as u64;

    let shell = Shell::get();
    let mut monitors = shell.monitors.lock().unwrap();
    let Some(rt) = monitors
        .values_mut()
        .find(|rt| rt.monitor == OsHandle(monitor.cast()))
    else {
        return STATUS_SUCCESS;
    };
    if let Some(old) = rt.worker.take() {
        old.stop();
    }

    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();
    let join = std::thread::spawn(move || frame_loop(swapchain, frame_event, luid, stop_thread));
    rt.worker = Some(Worker { stop, join: Some(join) });
    STATUS_SUCCESS
}

pub unsafe extern "C" fn evt_unassign(monitor: ffi::IDDCX_MONITOR) -> NTSTATUS {
    let shell = Shell::get();
    let worker = {
        let mut monitors = shell.monitors.lock().unwrap();
        monitors
            .values_mut()
            .find(|rt| rt.monitor == OsHandle(monitor.cast()))
            .and_then(|rt| rt.worker.take())
    };
    if let Some(worker) = worker {
        worker.stop();
    }
    STATUS_SUCCESS
}

fn create_device_on_luid(luid: u64) -> windows::core::Result<IDXGIDevice> {
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory1()?;
        let adapter_luid = windows::Win32::Foundation::LUID {
            LowPart: luid as u32,
            HighPart: (luid >> 32) as i32,
        };
        let adapter: IDXGIAdapter = factory.EnumAdapterByLuid(adapter_luid)?;
        let mut device: Option<ID3D11Device> = None;
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?;
        device.unwrap().cast::<IDXGIDevice>()
    }
}

fn frame_loop(swapchain: OsHandle, frame_event: OsHandle, luid: u64, stop: Arc<AtomicBool>) {
    let device = match create_device_on_luid(luid) {
        Ok(d) => d,
        Err(e) => {
            let code = e.code().0;
            tracelogging::write_event!(
                PROVIDER,
                "SwapChainDeviceCreateFailed",
                level(Error),
                i32("hresult", &code)
            );
            return;
        }
    };

    unsafe {
        let mut set_dev: ffi::IDARG_IN_SWAPCHAINSETDEVICE = zeroed();
        set_dev.pDevice = device.as_raw().cast();
        let status = bindings::swapchain_set_device(swapchain.0.cast(), &set_dev);
        if status != STATUS_SUCCESS {
            tracelogging::write_event!(
                PROVIDER,
                "SwapChainSetDeviceFailed",
                level(Error),
                i32("status", &status)
            );
            return;
        }

        while !stop.load(Ordering::SeqCst) {
            let mut out: ffi::IDARG_OUT_RELEASEANDACQUIREBUFFER = zeroed();
            let status = bindings::swapchain_release_and_acquire_buffer(
                swapchain.0.cast(),
                &mut out,
            );
            if status == STATUS_PENDING {
                // Bounded wait for the next frame; re-check stop on timeout.
                let _ = WaitForSingleObject(HANDLE(frame_event.0), 100);
                continue;
            }
            if status != STATUS_SUCCESS {
                tracelogging::write_event!(
                    PROVIDER,
                    "AcquireBufferFailed",
                    level(Warning),
                    i32("status", &status)
                );
                return;
            }
            // Phase 4 copies into the shared ring here. For now: done.
            bindings::swapchain_finished_processing_frame(swapchain.0.cast());
        }
    }
}

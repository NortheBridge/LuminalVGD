// SPDX-License-Identifier: AGPL-3.0-only
//! alttab-stress — the WGC-RELIABILITY.md §7 fullscreen-transition race
//! harness (libvirtualdisplay's `alttab_stress`, ported).
//!
//! Exclusive-fullscreen enter/leave on a virtual display makes the OS
//! rotate the IddCx swapchain (unassign/assign + the driver worker's
//! ACCESS_LOST teardown) while the game's and the capture client's D3D
//! devices churn on the same adapter — historically a deadlock/BSOD
//! class. This harness reproduces the trigger deliberately:
//!
//!   1. create an ephemeral LuminalVGD monitor and find its DXGI output
//!   2. consume the frame ring from a second thread (the capture client)
//!   3. hammer IDXGISwapChain::SetFullscreenState(true/false) from a
//!      window on that output (the game), timing every transition
//!
//! A watchdog fails the whole run loudly (exit 2) if any step stops
//! making progress — a wedged transition is exactly the §7 failure.
//!
//! ```text
//! alttab-stress [--iterations N]   default 50 round trips
//! ```

#![cfg(windows)]

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use luminal_driver_proto::{
    create_flags, err, CreateMonitorRequest, ModeSpec, LEASE_TIMEOUT_USE_DEFAULT,
    MAX_MODES_PER_MONITOR,
};
use luminal_vgd_host::device::{RingView, VgdDevice};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDeviceAndSwapChain, D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_MODE_DESC, DXGI_RATIONAL,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput, IDXGISwapChain, DXGI_PRESENT,
    DXGI_SWAP_CHAIN_DESC, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_CHAIN_FLAG_ALLOW_MODE_SWITCH,
    DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, PeekMessageW, RegisterClassW,
    SetForegroundWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW, MSG, PM_REMOVE, WINDOW_EX_STYLE,
    WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

const MODE: ModeSpec = ModeSpec { width: 1920, height: 1080, refresh_millihz: 60_000 };

/// No progress for this long on any step = the §7 wedge. Generous: a
/// healthy transition completes in well under a second.
const WEDGE_TIMEOUT: Duration = Duration::from_secs(15);

/// Consumer-side stall detector (same contract as vgd-probe --consume).
const DELIVERY_STALL: Duration = Duration::from_secs(5);

extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Device names of all desktop-attached DXGI outputs. The factory is
/// recreated per call — a cached one goes stale across display changes.
fn attached_outputs() -> Vec<([u16; 32], RECT)> {
    let mut out = Vec::new();
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
        return out;
    };
    let mut a = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(a) } {
        a += 1;
        let mut o = 0;
        while let Ok(output) = unsafe { adapter.EnumOutputs(o) } {
            o += 1;
            if let Ok(desc) = unsafe { output.GetDesc() } {
                if desc.AttachedToDesktop.as_bool() {
                    out.push((desc.DeviceName, desc.DesktopCoordinates));
                }
            }
        }
    }
    out
}

fn pump_messages() {
    let mut msg = MSG::default();
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

struct Progress {
    /// Milliseconds-since-start of the last progress bump.
    beat: AtomicU64,
    step: Mutex<String>,
    start: Instant,
}

impl Progress {
    fn bump(&self, step: &str) {
        *self.step.lock().unwrap() = step.to_string();
        self.beat
            .store(self.start.elapsed().as_millis() as u64, Ordering::Release);
    }
}

fn main() -> ExitCode {
    let mut iterations: u32 = 50;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--iterations" => {
                let Some(v) = it.next().and_then(|v| v.parse().ok()) else {
                    eprintln!("alttab-stress: --iterations needs a number");
                    return ExitCode::FAILURE;
                };
                iterations = v;
            }
            other => {
                eprintln!("alttab-stress: unknown argument {other}");
                eprintln!("usage: alttab-stress [--iterations N]");
                return ExitCode::FAILURE;
            }
        }
    }

    println!("[1/5] opening LuminalVGD control device…");
    let dev = match VgdDevice::open_first() {
        Ok(d) => Arc::new(d),
        Err(e) => {
            eprintln!("  cannot open device: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = dev.handshake() {
        eprintln!("  handshake failed: {e}");
        return ExitCode::FAILURE;
    }

    let session_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(1)
        | (std::process::id() as u64) << 48;

    let before = attached_outputs();

    println!(
        "[2/5] CREATE_MONITOR {}x{}@{}mHz (ephemeral, session {:#x})…",
        MODE.width, MODE.height, MODE.refresh_millihz, session_id
    );
    let mut modes = [ModeSpec::default(); MAX_MODES_PER_MONITOR as usize];
    modes[0] = MODE;
    let mut friendly_name = [0u16; 32];
    for (i, c) in "LuminalVGD AltTab".encode_utf16().enumerate() {
        friendly_name[i] = c;
    }
    let req = CreateMonitorRequest {
        session_id,
        display_id: 0,
        adapter_luid: 0,
        lease_timeout_ms: LEASE_TIMEOUT_USE_DEFAULT,
        bit_depth: 8,
        hdr: 0,
        edid_serial: 0,
        flags: create_flags::EPHEMERAL_IDENTITY,
        mode_count: 1,
        modes,
        physical_width_mm: 0,
        physical_height_mm: 0,
        friendly_name,
    };
    let reply = match dev.create_monitor(&req) {
        Ok(r) if r.result == err::OK => r,
        Ok(r) => {
            eprintln!("  driver refused: result {}", r.result);
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("  CREATE_MONITOR failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    // The monitor appearing on the desktop is asynchronous PnP; find the
    // output that wasn't there before.
    println!("[3/5] waiting for the new DXGI output…");
    let mut target: Option<([u16; 32], RECT)> = None;
    for _ in 0..100 {
        let now = attached_outputs();
        target = now
            .iter()
            .find(|(name, _)| !before.iter().any(|(b, _)| b == name))
            .copied();
        if target.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let Some((name, rect)) = target else {
        eprintln!("  virtual display never appeared as a DXGI output");
        let _ = dev.destroy_monitor(session_id);
        return ExitCode::FAILURE;
    };
    let name_end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    println!(
        "  {} at ({}, {})",
        String::from_utf16_lossy(&name[..name_end]),
        rect.left,
        rect.top
    );

    let progress = Arc::new(Progress {
        beat: AtomicU64::new(0),
        step: Mutex::new("startup".into()),
        start: Instant::now(),
    });
    progress.bump("startup");

    // Watchdog: a wedged SetFullscreenState/claim never returns — detect
    // the silence and fail the run loudly (that IS the §7 bug).
    {
        let progress = progress.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(500));
            let last = progress.beat.load(Ordering::Acquire);
            let now = progress.start.elapsed().as_millis() as u64;
            if now.saturating_sub(last) > WEDGE_TIMEOUT.as_millis() as u64 {
                eprintln!(
                    "alttab-stress WEDGED: no progress for {} s during '{}' ✘",
                    WEDGE_TIMEOUT.as_secs(),
                    progress.step.lock().unwrap()
                );
                std::process::exit(2);
            }
        });
    }

    // Capture-client stand-in: consume the ring exactly like the probe's
    // soak mode (monotonic delivery + stall detection) and keep the lease
    // pinged. Runs until the stress loop finishes.
    let stop = Arc::new(AtomicBool::new(false));
    let consumer = {
        let stop = stop.clone();
        let dev = dev.clone();
        let progress = progress.clone();
        let ring_slots = reply.ring_slots;
        std::thread::spawn(move || {
            let mut view = None;
            for _ in 0..50 {
                match RingView::open(session_id, ring_slots) {
                    Ok(v) => {
                        view = Some(v);
                        break;
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(100)),
                }
            }
            let Some(view) = view else {
                eprintln!("  (ring section never became mappable — consuming nothing)");
                return (0u64, 0u32);
            };
            let mut delivered = 0u64;
            let mut last_delivered = 0u64;
            let mut last_progress = Instant::now();
            let mut stalls = 0u32;
            let mut last_ping = Instant::now();
            while !stop.load(Ordering::Acquire) {
                while let Some(frame) = view.claim_latest() {
                    let fresh = frame.sequence > last_delivered;
                    view.release(frame.index);
                    if !fresh {
                        break;
                    }
                    last_delivered = frame.sequence;
                    delivered += 1;
                    last_progress = Instant::now();
                }
                let latest = view.header().latest_sequence;
                if latest <= last_delivered {
                    last_progress = Instant::now();
                } else if last_progress.elapsed() > DELIVERY_STALL {
                    stalls += 1;
                    eprintln!(
                        "  RING STALL: latest {latest} vs delivered {last_delivered} for >{} s",
                        DELIVERY_STALL.as_secs()
                    );
                    last_progress = Instant::now();
                }
                if last_ping.elapsed() > Duration::from_secs(1) {
                    let _ = dev.ping(session_id);
                    last_ping = Instant::now();
                    progress.bump("consumer");
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            (delivered, stalls)
        })
    };

    // The game stand-in: a swapchain window on the virtual output.
    println!("[4/5] running {iterations} exclusive-fullscreen round trips…");
    let run = || -> Result<(u128, u128, u32), String> {
        unsafe {
            let instance =
                GetModuleHandleW(None).map_err(|e| format!("GetModuleHandleW: {e}"))?;
            let class_name = wide("LuminalVgdAltTabStress");
            let wc = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                lpszClassName: PCWSTR(class_name.as_ptr()),
                ..Default::default()
            };
            if RegisterClassW(&wc) == 0 {
                return Err("RegisterClassW failed".into());
            }
            let title = wide("LuminalVGD alttab-stress");
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(class_name.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                rect.left + 50,
                rect.top + 50,
                1280,
                720,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .map_err(|e| format!("CreateWindowExW: {e}"))?;
            let _ = SetForegroundWindow(hwnd);
            pump_messages();

            let sc_desc = DXGI_SWAP_CHAIN_DESC {
                BufferDesc: DXGI_MODE_DESC {
                    Width: MODE.width,
                    Height: MODE.height,
                    RefreshRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    ..Default::default()
                },
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                OutputWindow: hwnd,
                Windowed: true.into(),
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                Flags: DXGI_SWAP_CHAIN_FLAG_ALLOW_MODE_SWITCH.0 as u32,
            };
            let mut swapchain: Option<IDXGISwapChain> = None;
            let mut device = None;
            let mut context = None;
            D3D11CreateDeviceAndSwapChain(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                Default::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                None,
                D3D11_SDK_VERSION,
                Some(&sc_desc),
                Some(&mut swapchain),
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .map_err(|e| format!("D3D11CreateDeviceAndSwapChain: {e}"))?;
            let swapchain = swapchain.ok_or("no swapchain")?;

            let mut max_enter = 0u128;
            let mut max_leave = 0u128;
            let mut errors = 0u32;
            let mut transition = |fullscreen: bool, label: &str| -> u128 {
                progress.bump(label);
                let t0 = Instant::now();
                let hr = swapchain.SetFullscreenState(fullscreen, None::<&IDXGIOutput>);
                pump_messages();
                let _ = swapchain.ResizeBuffers(
                    0,
                    0,
                    0,
                    DXGI_FORMAT_UNKNOWN,
                    DXGI_SWAP_CHAIN_FLAG(DXGI_SWAP_CHAIN_FLAG_ALLOW_MODE_SWITCH.0),
                );
                for _ in 0..3 {
                    let _ = swapchain.Present(0, DXGI_PRESENT(0));
                    pump_messages();
                    std::thread::sleep(Duration::from_millis(10));
                }
                if hr.is_err() {
                    errors += 1;
                    eprintln!("  {label} failed: {hr:?}");
                }
                progress.bump("between transitions");
                t0.elapsed().as_millis()
            };

            for i in 0..iterations {
                let enter = transition(true, "enter fullscreen");
                std::thread::sleep(Duration::from_millis(50));
                let leave = transition(false, "leave fullscreen");
                std::thread::sleep(Duration::from_millis(50));
                max_enter = max_enter.max(enter);
                max_leave = max_leave.max(leave);
                if (i + 1) % 10 == 0 {
                    println!(
                        "  [{:>3}/{}] max enter {} ms, max leave {} ms",
                        i + 1,
                        iterations,
                        max_enter,
                        max_leave
                    );
                }
            }
            let _ = swapchain.SetFullscreenState(false, None::<&IDXGIOutput>);
            pump_messages();
            Ok((max_enter, max_leave, errors))
        }
    };
    let result = run();

    stop.store(true, Ordering::Release);
    progress.bump("joining consumer");
    let (delivered, stalls) = consumer.join().unwrap_or((0, 0));

    println!("[5/5] DESTROY_MONITOR…");
    progress.bump("destroy monitor");
    match dev.destroy_monitor(session_id) {
        Ok(r) if r == err::OK => {}
        Ok(r) => eprintln!("  result {r}"),
        Err(e) => eprintln!("  failed: {e}"),
    }

    match result {
        Ok((max_enter, max_leave, errors)) if errors == 0 && stalls == 0 => {
            println!(
                "alttab-stress: {iterations} round trips, max enter {max_enter} ms, max leave {max_leave} ms, {delivered} frames consumed monotonically, 0 stalls ✔"
            );
            ExitCode::SUCCESS
        }
        Ok((max_enter, max_leave, errors)) => {
            eprintln!(
                "alttab-stress FAILED: {errors} transition error(s), {stalls} ring stall(s) (max enter {max_enter} ms, max leave {max_leave} ms, {delivered} frames) ✘"
            );
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("alttab-stress FAILED during setup: {e} ✘");
            ExitCode::FAILURE
        }
    }
}

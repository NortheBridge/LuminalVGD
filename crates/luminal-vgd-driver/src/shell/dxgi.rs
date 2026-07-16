// SPDX-License-Identifier: AGPL-3.0-only
//! DXGI render-adapter enumeration feeding `DeviceState::set_adapters`
//! (selection itself is core's `adapter::select_adapter`, tested).

use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE,
};

use luminal_vgd_core::adapter::AdapterInfo;

pub fn enumerate() -> Vec<AdapterInfo> {
    let mut out = Vec::new();
    let factory: IDXGIFactory1 = match unsafe { CreateDXGIFactory1() } {
        Ok(f) => f,
        Err(_) => return out,
    };
    for index in 0.. {
        let adapter = match unsafe { factory.EnumAdapters1(index) } {
            Ok(a) => a,
            Err(_) => break,
        };
        let Ok(desc) = (unsafe { adapter.GetDesc1() }) else {
            continue;
        };
        let luid = ((desc.AdapterLuid.HighPart as u32 as u64) << 32)
            | desc.AdapterLuid.LowPart as u64;
        let len = desc
            .Description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc.Description.len());
        out.push(AdapterInfo {
            luid,
            vram_bytes: desc.DedicatedVideoMemory as u64,
            name: String::from_utf16_lossy(&desc.Description[..len]),
            software: desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
        });
    }
    out
}

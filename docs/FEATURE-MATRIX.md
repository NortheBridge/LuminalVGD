# Feature Matrix — pf-vdisplay × SudoVDA → LuminalVGD

Disposition of every notable feature from the two source projects.
"Port" = re-specify and implement in Rust (behavior, not code).
"Inherit" = code lineage permitted (pf-vdisplay is MIT/Apache → AGPL-OK).

| Feature | Source | Disposition in LuminalVGD |
|---|---|---|
| All-Rust UMDF IddCx driver core | pf-vdisplay | **Inherit** — fork basis |
| Direct-to-encoder shared-texture/ring transport | pf-vdisplay | **Inherit**; primary mode |
| Single shared ABI crate (host+driver) | pf-vdisplay (`pf-driver-proto`) | **Inherit** as `luminal-driver-proto` |
| Build/sign/deploy scripting, FORCE_INTEGRITY clear | pf-vdisplay | **Inherit**, adapted to OV cert (TrustedPublisher-only seeding) |
| WGC/DDA fallback when driver absent | pf-vdisplay (host side) | **Inherit pattern**; hardened per WGC-RELIABILITY.md |
| Per-client virtual monitor create/destroy via control device | SudoVDA (Apollo integration) | **Port** as `CREATE_MONITOR`/`DESTROY_MONITOR` IOCTLs |
| Exact per-session mode (res/refresh/HDR), no giant mode list | SudoVDA | **Port**; single-entry mode list per monitor |
| Render adapter selection (`gpuName`, default = largest VRAM) | SudoVDA | **Port**; explicit `adapter_luid` param + same default; enables iGPU-capture hybrid setups |
| `maxMonitors` cap (default 10) | SudoVDA | **Port**; registry-configurable global cap |
| Driver watchdog (default 3 s, 0 = off) reaping orphaned monitors | SudoVDA | **Port** as `PING`-fed per-session watchdog |
| SDR 8/10-bit, HDR 10/12-bit depth options | SudoVDA | **Port** as `CREATE_MONITOR` params |
| HDR support gated on Win11 24H2 | SudoVDA (documented constraint) | **Port**; caps bit in handshake, installer note |
| High-res EDID (up to 8K/high refresh) | SudoVDA lineage | **Port**; generate EDID per created mode instead of static blob |
| Frame-generation-aware refresh doubling | LuminalVGD planned scope | **New**; host-side policy, driver honors mode |
| GPU-TDR/reset survival without WUDFHost wedge | LuminalVGD planned scope | **New**; ring-generation counter + bounded waits (DESIGN.md §3.3) |
| Recovery ladder + status/heartbeat IOCTL | LuminalVGD planned scope | **New**; `GET_STATUS`, reason-coded telemetry |
| Seamless, OS-silent WGC fallback with mid-session direct-encode restore | Product requirement (2026-07), DESIGN.md §2.1 | **New**; host-side controller: backoff probes, warm-WGC handover, no OS toast |

Explicitly **not** carried over:
- SudoVDA's `option.txt` static mode file and registry-driven monitor
  toggling (Device Manager enable/disable) — replaced by the session IOCTL
  model.
- SudoVDA C++ source — behavior is ported; code is not copied
  (language + provenance; see DESIGN.md §7).
- pf-vdisplay's self-signed cert Root-store seeding — replaced by OV
  signing with TrustedPublisher-only trust.

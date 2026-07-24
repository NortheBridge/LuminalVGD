<!-- SPDX-License-Identifier: AGPL-3.0-only -->
# Installing LuminalVGD

LuminalVGD is NortheBridge Foundation's virtual display driver for
[LuminalShine](https://github.com/NortheBridge/luminalshine). It creates
per-client virtual monitors with exact modes and HDR10 support, and
streams frames to LuminalShine through a shared-memory ring — no desktop
capture API in the hot path.

## Requirements

- Windows 11 x64 (build 22000+). **24H2 (build 26100+) is required for
  full HDR support**; SDR works on any Windows 11.
- LuminalShine 26.08.0-beta.2 or newer to actually stream (the driver
  alone just provides virtual displays). Older LuminalShine builds
  (including 26.08.0-beta.1) cannot complete the control-device
  handshake with this driver — its host must grant impersonation at
  open, which shipped in beta.2.

## Install

From an **elevated** PowerShell in the unpacked release folder:

```powershell
powershell -ExecutionPolicy Bypass -File .\install-driver.ps1 -Package .\driver-package -SeedTrustedPublisher
```

- `-SeedTrustedPublisher` adds the NortheBridge signing certificate to
  `LocalMachine\TrustedPublisher` so the install runs without a
  publisher-trust prompt. Only that store is touched — the certificate
  already chains to a public root. Omit the switch to review the prompt
  yourself.
- The script creates the `root\luminal_vgd` device. "Luminal Video
  Graphics Display" appears under **Display adapters** in Device Manager;
  no monitor is shown until LuminalShine (or `vgd-probe`) creates one.

## Verify

```powershell
pnputil /enum-devices /deviceid root\luminal_vgd
```

should list the device as started. LuminalShine logs
`Virtual-display backend: LuminalVGD (first-party IddCx driver)` at
startup once it sees the driver.

## Security notes

- The driver's control interface (create/destroy/lease virtual monitors)
  accepts connections **only from SYSTEM or elevated Administrators** —
  unprivileged processes are refused at open.
- Driver DLL and catalog are Authenticode-signed by "NortheBridge
  Foundation" and timestamped; verify with
  `Get-AuthenticodeSignature .\driver-package\luminal_vgd_driver.dll`.

## Uninstall

```powershell
powershell -ExecutionPolicy Bypass -File .\uninstall-driver.ps1
```

Add `-RemovePublisherCert` to also remove the signing certificate from
`TrustedPublisher`. A reboot fully unloads a driver instance that was
running during uninstall.

## Troubleshooting

- **Device shows a problem code after install**: reboot — the UMDF DLL
  copy can queue behind a running instance from a previous version.
- **LuminalShine falls back to SudoVDA/WGC**: check the device state in
  Device Manager, then `Get-AuthenticodeSignature` on the installed
  package; an install prompt that was cancelled leaves the old driver
  active.
- Diagnostics: the driver traces to ETW provider
  `NortheBridge.LuminalVGD` {c501990d-df12-5581-60a8-f55d593d7f7c}.

# SPDX-License-Identifier: AGPL-3.0-only
# Install the LuminalVGD driver package (DESIGN.md §6): OS floor check,
# signature verification, optional TrustedPublisher seeding, driver-package
# add, and root\luminal_vgd devnode creation. Uninstall with
# uninstall-driver.ps1.
#Requires -RunAsAdministrator
param(
    [string]$Package = (Join-Path $PSScriptRoot "..\target\driver-package"),
    # Seed the package's signing certificate into
    # LocalMachine\TrustedPublisher (ONLY TrustedPublisher — the OV cert
    # already chains to a trusted root; the Root store is never touched).
    # Without it, Windows shows a publisher-trust prompt during install.
    [switch]$SeedTrustedPublisher
)
$ErrorActionPreference = 'Stop'

# OS floor (DESIGN.md §6): Windows 11 required; 24H2+ for full HDR.
$build = [Environment]::OSVersion.Version.Build
if ($build -lt 22000) {
    throw "LuminalVGD requires Windows 11 (build 22000+); this is build $build."
}
if ($build -lt 26100) {
    Write-Warning "Windows 11 24H2 (build 26100+) is required for full HDR support; this is build $build. SDR streaming works."
}

$inf = Join-Path $Package 'luminalvgd.inf'
$cat = Join-Path $Package 'luminalvgd.cat'
$dll = Join-Path $Package 'luminal_vgd_driver.dll'
foreach ($f in @($inf, $cat, $dll)) {
    if (-not (Test-Path $f)) { throw "missing $f — run scripts\build-driver.cmd first" }
}

$sig = Get-AuthenticodeSignature $cat
if ($sig.Status -ne 'Valid') {
    throw "catalog signature is '$($sig.Status)' — sign the package first (docs/BUILDING.md, Signing)"
}
$inTrusted = Get-ChildItem Cert:\LocalMachine\TrustedPublisher |
    Where-Object Thumbprint -eq $sig.SignerCertificate.Thumbprint
if (-not $inTrusted -and $SeedTrustedPublisher) {
    Write-Host "Seeding signer into LocalMachine\TrustedPublisher: $($sig.SignerCertificate.Subject)"
    $store = New-Object System.Security.Cryptography.X509Certificates.X509Store('TrustedPublisher', 'LocalMachine')
    $store.Open('ReadWrite')
    try { $store.Add($sig.SignerCertificate) } finally { $store.Close() }
    $inTrusted = $true
}
if (-not $inTrusted) {
    Write-Warning "Signer not in LocalMachine\TrustedPublisher — Windows will show an install prompt (re-run with -SeedTrustedPublisher to install silently)."
}

Write-Host "Adding driver package…"
pnputil /add-driver $inf /install
# 0 = added; 3010 = success, reboot pending; 259 = ERROR_NO_MORE_ITEMS
# (package already published, nothing newly added — the force-rebind below
# still installs the staged binary on the device).
if ($LASTEXITCODE -notin 0, 3010, 259) { throw "pnputil /add-driver failed ($LASTEXITCODE)" }
$rebootPending = ($LASTEXITCODE -eq 3010)

# Root devnodes get SetupDi-generated instance ids (ROOT\DISPLAY\000x);
# identify ours by hardware id.
function Get-LuminalDevice {
    Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object {
        (Get-PnpDeviceProperty -InstanceId $_.InstanceId -KeyName 'DEVPKEY_Device_HardwareIds' -ErrorAction SilentlyContinue).Data -contains 'root\luminal_vgd'
    }
}
$existing = Get-LuminalDevice
if ($existing) {
    Write-Host "Devnode already present: $($existing.InstanceId) [$($existing.Status)]"
} else {
    Write-Host "Creating root\luminal_vgd devnode…"
    # devcon-install equivalent via SetupDi (devcon no longer ships in the eWDK).
    Add-Type -Namespace LuminalVgd -Name DevNode -MemberDefinition @'
[DllImport("setupapi.dll", SetLastError = true, CharSet = CharSet.Unicode)]
public static extern IntPtr SetupDiCreateDeviceInfoList(ref Guid ClassGuid, IntPtr hwndParent);
[DllImport("setupapi.dll", SetLastError = true, CharSet = CharSet.Unicode)]
public static extern bool SetupDiCreateDeviceInfoW(IntPtr DeviceInfoSet, string DeviceName, ref Guid ClassGuid, string DeviceDescription, IntPtr hwndParent, int CreationFlags, ref SP_DEVINFO_DATA DeviceInfoData);
[DllImport("setupapi.dll", SetLastError = true, CharSet = CharSet.Unicode)]
public static extern bool SetupDiSetDeviceRegistryPropertyW(IntPtr DeviceInfoSet, ref SP_DEVINFO_DATA DeviceInfoData, int Property, byte[] PropertyBuffer, int PropertyBufferSize);
[DllImport("setupapi.dll", SetLastError = true)]
public static extern bool SetupDiCallClassInstaller(int InstallFunction, IntPtr DeviceInfoSet, ref SP_DEVINFO_DATA DeviceInfoData);
[DllImport("setupapi.dll", SetLastError = true)]
public static extern bool SetupDiDestroyDeviceInfoList(IntPtr DeviceInfoSet);
[StructLayout(LayoutKind.Sequential)]
public struct SP_DEVINFO_DATA { public int cbSize; public Guid ClassGuid; public int DevInst; public IntPtr Reserved; }
'@
    $displayClass = [Guid]'4d36e968-e325-11ce-bfc1-08002be10318'
    $DICD_GENERATE_ID = 0x1
    $SPDRP_HARDWAREID = 0x1
    $DIF_REGISTERDEVICE = 0x19

    $set = [LuminalVgd.DevNode]::SetupDiCreateDeviceInfoList([ref]$displayClass, [IntPtr]::Zero)
    if ($set -eq [IntPtr]::Zero -or $set -eq [IntPtr]::new(-1)) { throw "SetupDiCreateDeviceInfoList failed" }
    try {
        $data = New-Object LuminalVgd.DevNode+SP_DEVINFO_DATA
        $data.cbSize = [Runtime.InteropServices.Marshal]::SizeOf($data)
        if (-not [LuminalVgd.DevNode]::SetupDiCreateDeviceInfoW($set, 'Display', [ref]$displayClass, 'Luminal Video Graphics Display', [IntPtr]::Zero, $DICD_GENERATE_ID, [ref]$data)) {
            throw "SetupDiCreateDeviceInfo failed: $([Runtime.InteropServices.Marshal]::GetLastWin32Error())"
        }
        # REG_MULTI_SZ: "root\luminal_vgd" + double NUL terminator.
        $hwid = [Text.Encoding]::Unicode.GetBytes("root\luminal_vgd`0`0")
        if (-not [LuminalVgd.DevNode]::SetupDiSetDeviceRegistryPropertyW($set, [ref]$data, $SPDRP_HARDWAREID, $hwid, $hwid.Length)) {
            throw "SetupDiSetDeviceRegistryProperty failed: $([Runtime.InteropServices.Marshal]::GetLastWin32Error())"
        }
        if (-not [LuminalVgd.DevNode]::SetupDiCallClassInstaller($DIF_REGISTERDEVICE, $set, [ref]$data)) {
            throw "SetupDiCallClassInstaller(DIF_REGISTERDEVICE) failed: $([Runtime.InteropServices.Marshal]::GetLastWin32Error())"
        }
    } finally {
        [void][LuminalVgd.DevNode]::SetupDiDestroyDeviceInfoList($set)
    }
    Write-Host "Devnode registered."
}

# Registering a devnode does not trigger driver matching, and
# /scan-devices is unreliable for root-enumerated devices — do what
# devcon does: UpdateDriverForPlugAndPlayDevices on the hardware id.
Write-Host "Binding driver to root\luminal_vgd…"
Add-Type -Namespace LuminalVgd -Name NewDev -MemberDefinition @'
[DllImport("newdev.dll", SetLastError = true, CharSet = CharSet.Unicode)]
public static extern bool UpdateDriverForPlugAndPlayDevicesW(IntPtr hwndParent, string HardwareId, string FullInfPath, uint InstallFlags, out bool bRebootRequired);
'@
$INSTALLFLAG_FORCE = 0x1
$reboot = $false
$infFull = (Resolve-Path $inf).Path
if (-not [LuminalVgd.NewDev]::UpdateDriverForPlugAndPlayDevicesW([IntPtr]::Zero, 'root\luminal_vgd', $infFull, $INSTALLFLAG_FORCE, [ref]$reboot)) {
    throw "UpdateDriverForPlugAndPlayDevices failed: $([Runtime.InteropServices.Marshal]::GetLastWin32Error())"
}
if ($reboot -or $rebootPending) { Write-Warning "Windows reports a reboot is required to finish the install." }

Start-Sleep -Seconds 3
$dev = Get-LuminalDevice
$dev | Format-Table InstanceId, Status, FriendlyName
if ($dev) {
    $inf = (Get-PnpDeviceProperty -InstanceId $dev.InstanceId -KeyName 'DEVPKEY_Device_DriverInfPath' -ErrorAction SilentlyContinue).Data
    if ($inf) { Write-Host "Driver bound: $inf" } else { Write-Warning "devnode present but no driver bound yet — re-run 'pnputil /scan-devices'" }
}
Write-Host "Verify with: cargo run -p vgd-probe --release"

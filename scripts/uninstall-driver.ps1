# SPDX-License-Identifier: AGPL-3.0-only
# Uninstall LuminalVGD (DESIGN.md §6): remove the root\luminal_vgd devnode,
# delete the driver package from the DriverStore, and optionally remove the
# NortheBridge signing certificate from LocalMachine\TrustedPublisher.
#Requires -RunAsAdministrator
param(
    # Also remove the signer certificate seeded by install-driver.ps1
    # -SeedTrustedPublisher. Leave it when other NortheBridge software is
    # installed.
    [switch]$RemovePublisherCert
)
$ErrorActionPreference = 'Stop'

function Get-LuminalDevice {
    Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object {
        (Get-PnpDeviceProperty -InstanceId $_.InstanceId -KeyName 'DEVPKEY_Device_HardwareIds' -ErrorAction SilentlyContinue).Data -contains 'root\luminal_vgd'
    }
}

# 1. Remove the devnode(s). /remove-device exists on Win11 pnputil.
$devices = @(Get-LuminalDevice)
if ($devices.Count -eq 0) {
    Write-Host "No root\luminal_vgd devnode present."
} else {
    foreach ($dev in $devices) {
        Write-Host "Removing devnode $($dev.InstanceId)…"
        pnputil /remove-device $dev.InstanceId
        if ($LASTEXITCODE -notin 0, 3010) { Write-Warning "pnputil /remove-device returned $LASTEXITCODE" }
    }
}

# 2. Delete every published luminalvgd.inf package (oemXX.inf) from the
# DriverStore. /uninstall detaches it from any remaining devices first.
$published = pnputil /enum-drivers | Out-String
$oemInfs = @()
$current = $null
foreach ($line in ($published -split "`r?`n")) {
    if ($line -match 'Published Name:\s+(oem\d+\.inf)') { $current = $Matches[1] }
    if ($line -match 'Original Name:\s+luminalvgd\.inf' -and $current) { $oemInfs += $current; $current = $null }
}
if ($oemInfs.Count -eq 0) {
    Write-Host "No luminalvgd driver package in the DriverStore."
} else {
    foreach ($oem in $oemInfs) {
        Write-Host "Deleting driver package $oem…"
        pnputil /delete-driver $oem /uninstall /force
        if ($LASTEXITCODE -notin 0, 3010) { Write-Warning "pnputil /delete-driver $oem returned $LASTEXITCODE" }
    }
}

# 3. Optional: remove the signer certificate from TrustedPublisher.
if ($RemovePublisherCert) {
    $subjects = Get-ChildItem Cert:\LocalMachine\TrustedPublisher |
        Where-Object Subject -like '*NortheBridge*'
    if ($subjects) {
        foreach ($cert in $subjects) {
            Write-Host "Removing TrustedPublisher cert: $($cert.Subject) [$($cert.Thumbprint)]"
            Remove-Item -Path "Cert:\LocalMachine\TrustedPublisher\$($cert.Thumbprint)" -Confirm:$false
        }
    } else {
        Write-Host "No NortheBridge certificate in LocalMachine\TrustedPublisher."
    }
}

Write-Host "Uninstall complete. A reboot fully unloads a previously running driver instance."

# SPDX-License-Identifier: AGPL-3.0-only
# Clear IMAGE_DLLCHARACTERISTICS_FORCE_INTEGRITY (0x0080) from a PE file.
# wdk-build links with /INTEGRITYCHECK, which requires a Microsoft-rooted
# signature at load time; an OV-signed driver must not carry the bit
# (DESIGN.md §6). Must run before signing — it changes the file hash.
param([Parameter(Mandatory)][string]$Path)
$ErrorActionPreference = 'Stop'

$bytes = [IO.File]::ReadAllBytes($Path)
$peOffset = [BitConverter]::ToInt32($bytes, 0x3C)
if ($bytes[$peOffset] -ne 0x50 -or $bytes[$peOffset + 1] -ne 0x45) { throw "$Path is not a PE file" }
$magic = [BitConverter]::ToUInt16($bytes, $peOffset + 24)
if ($magic -ne 0x20B) { throw "expected PE32+ (found magic $magic)" }
# DllCharacteristics: PE sig(4) + COFF(20) + offset 70 into optional header.
$dcOffset = $peOffset + 4 + 20 + 70
$dc = [BitConverter]::ToUInt16($bytes, $dcOffset)
if ($dc -band 0x0080) {
    $dc = $dc -band (-bnot 0x0080)
    $bytes[$dcOffset] = [byte]($dc -band 0xFF)
    $bytes[$dcOffset + 1] = [byte]($dc -shr 8)
    [IO.File]::WriteAllBytes($Path, $bytes)
    Write-Host "FORCE_INTEGRITY cleared on $Path"
} else {
    Write-Host "FORCE_INTEGRITY already clear on $Path"
}

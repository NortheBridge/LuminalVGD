# SPDX-License-Identifier: AGPL-3.0-only
# Stage a signed LuminalVGD release archive (DESIGN.md §6).
#
# Flow (docs/BUILDING.md §Releasing):
#   1. $env:LUMINAL_VGD_BUILD = <build>; scripts\build-driver.cmd
#   2. sign luminal_vgd_driver.dll + luminalvgd.cat (eSigner, human-attended)
#   3. scripts\package-release.ps1 -Version v0.1.0-alpha.1
#   4. gh release create <tag> target\release-artifacts\* --prerelease ...
#
# Signed artifacts are release assets ONLY — never committed to the repo.
param(
    [Parameter(Mandatory)][string]$Version,
    [string]$Package = (Join-Path $PSScriptRoot "..\target\driver-package"),
    [string]$OutDir = (Join-Path $PSScriptRoot "..\target\release-artifacts")
)
$ErrorActionPreference = 'Stop'

$repo = Resolve-Path (Join-Path $PSScriptRoot '..')
$inf = Join-Path $Package 'luminalvgd.inf'
$cat = Join-Path $Package 'luminalvgd.cat'
$dll = Join-Path $Package 'luminal_vgd_driver.dll'
foreach ($f in @($inf, $cat, $dll)) {
    if (-not (Test-Path $f)) { throw "missing $f — run scripts\build-driver.cmd first" }
}

# Release gate: both binaries carry a valid, timestamped signature.
foreach ($f in @($dll, $cat)) {
    $sig = Get-AuthenticodeSignature $f
    if ($sig.Status -ne 'Valid') { throw "$f signature is '$($sig.Status)' — sign the package first" }
    if (-not $sig.TimeStamperCertificate) { throw "$f is signed but not timestamped — re-sign with /tr (RFC3161)" }
    Write-Host ("OK  {0}  [{1}]" -f (Split-Path $f -Leaf), $sig.SignerCertificate.Subject)
}

# FORCE_INTEGRITY must be clear in the shipped DLL (§6): verify, don't fix
# — clearing after signing would break the signature.
$bytes = [IO.File]::ReadAllBytes($dll)
$peOffset = [BitConverter]::ToInt32($bytes, 0x3C)
$dc = [BitConverter]::ToUInt16($bytes, $peOffset + 4 + 20 + 70)
if ($dc -band 0x0080) { throw "FORCE_INTEGRITY still set on the signed DLL — rebuild (build-driver.cmd clears it pre-signing)" }

if (Test-Path $OutDir) { Remove-Item -Recurse -Force $OutDir -Confirm:$false }
New-Item -ItemType Directory -Force $OutDir | Out-Null

$stage = Join-Path $OutDir "LuminalVGD-$Version-x64"
New-Item -ItemType Directory -Force (Join-Path $stage 'driver-package') | Out-Null
Copy-Item $inf, $cat, $dll (Join-Path $stage 'driver-package')
Copy-Item (Join-Path $PSScriptRoot 'install-driver.ps1') $stage
Copy-Item (Join-Path $PSScriptRoot 'uninstall-driver.ps1') $stage
Copy-Item (Join-Path $repo 'LICENSE') $stage
Copy-Item (Join-Path $repo 'THIRD-PARTY-NOTICES.md') $stage
Copy-Item (Join-Path $repo 'docs\INSTALL.md') $stage

$zip = Join-Path $OutDir "LuminalVGD-$Version-x64.zip"
Compress-Archive -Path "$stage\*" -DestinationPath $zip -CompressionLevel Optimal
Remove-Item -Recurse -Force $stage -Confirm:$false

$hash = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
"${hash}  $(Split-Path $zip -Leaf)" | Set-Content (Join-Path $OutDir 'SHA256SUMS')

Write-Host ""
Write-Host "Release artifacts staged:"
Get-ChildItem $OutDir | Format-Table Name, Length
Write-Host "Publish with:"
Write-Host "  gh release create $Version `"$zip`" `"$(Join-Path $OutDir 'SHA256SUMS')`" --prerelease --title `"LuminalVGD $Version`" --notes-file <notes.md>"

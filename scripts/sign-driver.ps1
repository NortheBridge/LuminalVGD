# SPDX-License-Identifier: AGPL-3.0-only
# Scan + sign the staged driver package with the NortheBridge OV cert via
# SSL.com eSigner.
#
# The account has eSigner's Malware Blocker enabled: every file hash must
# be pre-scanned (CodeSignTool scan_code) or signing — including
# signtool/CKA — is refused. CodeSignTool 1.3.x requires credentials as
# CLI options, so this script prompts for them at runtime (password
# hidden) and forwards them to SSL.com's tool only. Nothing is stored.
param(
    [string]$Package = (Join-Path $PSScriptRoot "..\target\driver-package"),
    [string]$CodeSignToolDir = "$env:USERPROFILE\CodeSignTool",
    [string]$CertSha1 = 'BE990312326FE00EB6400312286A7E307C5D65C0'
)
$ErrorActionPreference = 'Stop'

$dll = Join-Path $Package 'luminal_vgd_driver.dll'
$cat = Join-Path $Package 'luminalvgd.cat'
foreach ($f in @($dll, $cat)) {
    if (-not (Test-Path $f)) { throw "missing $f — run scripts\build-driver.cmd first" }
}
# Invoke the jar directly (the .bat just wraps it): passwords with cmd
# metacharacters survive PowerShell→exe argument passing, but not cmd/batch
# parsing.
$java = Join-Path $CodeSignToolDir 'jdk-11.0.2\bin\java.exe'
$jar = Join-Path $CodeSignToolDir 'jar\code_sign_tool-1.3.2.jar'
foreach ($f in @($java, $jar)) {
    if (-not (Test-Path $f)) { throw "CodeSignTool component not found: $f" }
}

$user = Read-Host 'SSL.com username'
$passSecure = Read-Host 'SSL.com password' -AsSecureString
$pass = [Runtime.InteropServices.Marshal]::PtrToStringBSTR(
    [Runtime.InteropServices.Marshal]::SecureStringToBSTR($passSecure))
$totpSecure = Read-Host 'eSigner TOTP secret (from the dashboard QR; hidden)' -AsSecureString
$totp = [Runtime.InteropServices.Marshal]::PtrToStringBSTR(
    [Runtime.InteropServices.Marshal]::SecureStringToBSTR($totpSecure))

Push-Location $CodeSignToolDir
try {
    Write-Host "`n=== Looking up eSigner credential id ===" -ForegroundColor Cyan
    $credOut = & $java -jar $jar get_credential_ids "-username=$user" "-password=$pass" 2>&1
    $credOut | Write-Host
    # Output contains lines like "- <uuid>"; collect uuid-shaped tokens.
    $ids = @([regex]::Matches(($credOut -join "`n"),
        '[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}') |
        ForEach-Object Value | Select-Object -Unique)
    if ($ids.Count -eq 0) {
        throw 'no eSigner credential ids returned — is the eSigner TOTP enrolled for this certificate in the SSL.com dashboard?'
    }
    $credId = $ids[0]
    if ($ids.Count -gt 1) {
        Write-Host "Multiple credentials found:" -ForegroundColor Yellow
        $ids | ForEach-Object { Write-Host "  $_" }
        $credId = Read-Host "Credential id to use [$($ids[0])]"
        if (-not $credId) { $credId = $ids[0] }
    }
    Write-Host "Using credential $credId"

    foreach ($file in @($dll, $cat)) {
        Write-Host "`n=== Malware pre-scan: $(Split-Path $file -Leaf) ===" -ForegroundColor Cyan
        $scanOut = & $java -jar $jar scan_code "-username=$user" "-password=$pass" `
            "-credential_id=$credId" "-program_name=LuminalVGD" "-input_file_path=$file" 2>&1
        $scanOut | Write-Host
        if ($LASTEXITCODE -ne 0 -or ($scanOut -join ' ') -match 'Error') {
            throw "scan_code failed for $file"
        }
    }

    # Sign with CodeSignTool itself (jsign-based; handles .dll and .cat,
    # pairs correctly with the malware-scan records, timestamps via
    # SSL.com). -override signs in place.
    foreach ($file in @($dll, $cat)) {
        Write-Host "`n=== eSigner signing: $(Split-Path $file -Leaf) ===" -ForegroundColor Cyan
        $signArgs = @('sign', "-username=$user", "-password=$pass",
            "-credential_id=$credId", "-program_name=LuminalVGD",
            "-input_file_path=$file", '-override')
        if ($totp) { $signArgs += "-totp_secret=$totp" }
        $signOut = & $java -jar $jar @signArgs 2>&1
        $signOut | Write-Host
        if ($LASTEXITCODE -ne 0 -or ($signOut -join ' ') -match 'Error') {
            throw "eSigner signing failed for $file"
        }
    }
} finally {
    Pop-Location
    $pass = $null
    $totp = $null
}

Write-Host "`nSigned. Verifying…" -ForegroundColor Cyan
Get-AuthenticodeSignature $dll, $cat | Format-Table Path, Status, StatusMessage
Write-Host "Next: elevated PowerShell → scripts\install-driver.ps1"

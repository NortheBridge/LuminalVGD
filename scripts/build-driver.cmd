@echo off
rem SPDX-License-Identifier: AGPL-3.0-only
rem Build the LuminalVGD driver DLL and stage the installable package at
rem target\driver-package (unsigned — signing is a human-attended eSigner
rem step, docs/BUILDING.md §Signing).
rem
rem Environment (docs/BUILDING.md):
rem   EWDK_ROOT      root of the eWDK (default C:\)
rem   LIBCLANG_PATH  LLVM 21.x bin dir (LLVM 22 breaks wdk-sys bindgen)
setlocal enabledelayedexpansion

set "REPO=%~dp0.."
if not defined EWDK_ROOT set "EWDK_ROOT=C:\"
if not defined LIBCLANG_PATH set "LIBCLANG_PATH=%USERPROFILE%\clang+llvm-21.1.2-x86_64-pc-windows-msvc\bin"
if not exist "%LIBCLANG_PATH%\libclang.dll" (
    echo error: libclang.dll not found under "%LIBCLANG_PATH%" — set LIBCLANG_PATH to an LLVM 21.x bin directory.
    exit /b 1
)

call "%EWDK_ROOT%BuildEnv\SetupBuildEnv.cmd" amd64 || exit /b 1
set "PATH=%LIBCLANG_PATH%;!PATH!"
rem The eWDK env is created with -winsdk=none; rustc needs the SDK libs.
set "LIB=!WindowsSdkDir!Lib\!Version_Number!\um\x64;!WindowsSdkDir!Lib\!Version_Number!\ucrt\x64;!LIB!"

cd /d "%REPO%" || exit /b 1
cargo build -p luminal-vgd-driver --features shell --release || exit /b 1

set "PKG=%REPO%\target\driver-package"
if exist "%PKG%" rd /s /q "%PKG%"
mkdir "%PKG%" || exit /b 1
copy /y "%REPO%\target\release\luminal_vgd_driver.dll" "%PKG%\" >nul || exit /b 1
copy /y "%REPO%\packaging\luminalvgd.inf" "%PKG%\" >nul || exit /b 1

rem Clear IMAGE_DLLCHARACTERISTICS_FORCE_INTEGRITY (0x0080): the wdk-build
rem link line sets /INTEGRITYCHECK, which only Microsoft-rooted signatures
rem satisfy — our OV signature would fail to load (DESIGN.md §6).
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0clear-force-integrity.ps1" "%PKG%\luminal_vgd_driver.dll" || exit /b 1

rem DriverVer convention (docs/BUILDING.md §Releasing):
rem   Stamped builds (LUMINAL_VGD_BUILD set, i.e. signing/release rounds):
rem     <LUMINAL_VGD_VERSION>.<LUMINAL_VGD_BUILD>, default version 0.1.0 —
rem     Device Manager shows e.g. 0.1.0.8, matching the handshake build
rem     and the vX.Y.Z release tag. DRIVER_BUILD bumps every signing
rem     round, so releases always outrank each other.
rem   Unstamped dev builds: date/time-derived 100.YYMM.DDHH.MMSS so every
rem     throwaway rebuild outranks the previous one (`-v *` can collide,
rem     pnputil then skips the update and the new binary never reaches
rem     the device). The 100. prefix keeps dev builds above any release
rem     scheme on dev boxes; release validation uninstalls first.
if not defined LUMINAL_VGD_VERSION set "LUMINAL_VGD_VERSION=0.1.0"
if defined LUMINAL_VGD_BUILD (
    set "DRIVERVER=%LUMINAL_VGD_VERSION%.%LUMINAL_VGD_BUILD%"
) else (
    for /f %%v in ('powershell -NoProfile -Command "$d=Get-Date; '100.{0:00}{1:00}.{2:00}{3:00}.{4:00}{5:00}' -f ($d.Year%%100),$d.Month,$d.Day,$d.Hour,$d.Minute,$d.Second"') do set "DRIVERVER=%%v"
)
stampinf -f "%PKG%\luminalvgd.inf" -d * -v %DRIVERVER% -a amd64 || exit /b 1
echo Stamped DriverVer %DRIVERVER%
"!WindowsSdkDir!bin\!Version_Number!\x86\Inf2Cat.exe" /driver:"%PKG%" /os:10_NI_X64 || exit /b 1

echo.
echo Package staged at %PKG% (unsigned).
echo Next: sign DLL + catalog with the NortheBridge OV cert via eSigner:
echo   signtool sign /sha1 BE990312326FE00EB6400312286A7E307C5D65C0 /fd SHA256 /td SHA256 /tr http://ts.ssl.com "%PKG%\luminal_vgd_driver.dll" "%PKG%\luminalvgd.cat"
echo Then install:  powershell -File "%REPO%\scripts\install-driver.ps1"
endlocal

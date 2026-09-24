# install-context-menu.ps1
#
# Registers the AAHL Explorer context menu for the CURRENT USER only (HKCU):
# no elevation, no reboot, no Explorer restart. It mirrors what the DLL's
# DllRegisterServer does, using reg.exe so the paths with '*' behave exactly
# as the DLL's RegCreateKeyExW calls.
#
# Usage:
#   .\install-context-menu.ps1 [-BinDir <path>]
#
#   -BinDir  directory that holds aahl.exe, aahl-gui.exe and aahl_shellext.dll
#            (default: the directory this script lives in)
#
# Exit codes: 0 = registered, 1 = error.

param(
    [string]$BinDir = ""
)

$ErrorActionPreference = "Stop"

$script:CLSID = "{FF2729B8-37AC-4416-B770-EF81D8E5F168}"
$script:AAHL_KEY = "Software\AAHL"

if ($BinDir -eq "") {
    $BinDir = Split-Path -Parent $MyInvocation.MyCommand.Path
}
if (-not (Test-Path -LiteralPath $BinDir -PathType Container)) {
    Write-Error "directory not found: $BinDir"
    exit 1
}
$BinDir = (Resolve-Path -LiteralPath $BinDir).Path

$dll = Join-Path $BinDir "aahl_shellext.dll"
$cli = Join-Path $BinDir "aahl.exe"
$gui = Join-Path $BinDir "aahl-gui.exe"
foreach ($file in @($dll, $cli, $gui)) {
    if (-not (Test-Path -LiteralPath $file -PathType Leaf)) {
        Write-Error "missing file: $file`nInstall the AAHL binaries next to this script (or pass -BinDir) and try again."
        exit 1
    }
}

# Add a registry value idempotently. $Name "" means the (default) value.
function Invoke-RegAdd([string]$Path, [string]$Name, [string]$Value) {
    $regArgs = @("add", $Path)
    if ($Name -ne "") { $regArgs += @("/v", $Name) } else { $regArgs += "/ve" }
    $regArgs += @("/d", $Value, "/f")
    & reg.exe $regArgs
    if ($LASTEXITCODE -ne 0) {
        throw "reg add failed for $Path"
    }
}

try {
    $clsidKey = "HKCU\Software\Classes\CLSID\$CLSID"
    Invoke-RegAdd $clsidKey "" "AAHL Context Menu Shell Extension"
    Invoke-RegAdd "$clsidKey\InprocServer32" "" $dll
    Invoke-RegAdd "$clsidKey\InprocServer32" "ThreadingModel" "Apartment"
    Invoke-RegAdd "HKCU\Software\Classes\*\shellex\ContextMenuHandlers\AAHL" "" $CLSID
    Invoke-RegAdd "HKCU\Software\Classes\Directory\shellex\ContextMenuHandlers\AAHL" "" $CLSID
    Invoke-RegAdd "HKCU\$AAHL_KEY" "" $BinDir

    Write-Host "AAHL context menu registered for $env:USERNAME."
    Write-Host "  Shell DLL:      $dll"
    Write-Host "  Engine (CLI):   $cli"
    Write-Host "  GUI:            $gui"
    Write-Host "Right-clicking files and folders in NEW Explorer windows shows the menu."
    exit 0
}
catch {
    Write-Error $_
    exit 1
}
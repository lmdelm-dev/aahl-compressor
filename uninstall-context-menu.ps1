# uninstall-context-menu.ps1
#
# Removes the AAHL Explorer context menu registration for the CURRENT USER
# (HKCU). Idempotent: already-missing keys are not an error. The AAHL
# binaries are left in place.
#
# Usage:
#   .\uninstall-context-menu.ps1
#
# Exit codes: 0 = unregistered, 1 = something is still registered.

$ErrorActionPreference = "Stop"

$script:CLSID = "{FF2729B8-37AC-4416-B770-EF81D8E5F168}"

$paths = @(
    "HKCU\Software\Classes\CLSID\$CLSID",
    "HKCU\Software\Classes\*\shellex\ContextMenuHandlers\AAHL",
    "HKCU\Software\Classes\Directory\shellex\ContextMenuHandlers\AAHL",
    "HKCU\Software\AAHL"
)

foreach ($path in $paths) {
    # Exit code 1 from reg.exe = key not found; that is fine (idempotent).
    & reg.exe delete $path /f 2>$null | Out-Null
    if ($LASTEXITCODE -ne 0 -and $LASTEXITCODE -ne 1) {
        Write-Error "reg delete failed for $path (exit $LASTEXITCODE)"
        exit 1
    }
}

$gone = $true
foreach ($path in $paths) {
    if (Test-Path -LiteralPath $path) {
        $gone = $false
        Write-Warning "still present: $path"
    }
}
if ($gone) {
    Write-Host "AAHL context menu unregistered for $env:USERNAME."
    Write-Host "The binaries were left in place; you may delete them manually."
    exit 0
}
exit 1
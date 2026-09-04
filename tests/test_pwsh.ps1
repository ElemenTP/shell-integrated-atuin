# PowerShell integration test for the atuin-native module.
#
# Usage:
#   $env:DLL_DIR = "/path/to/pwsh_src/AtuinNative/bin/Release/net8.0"
#   pwsh -NoProfile -File tests/test_pwsh.ps1

param(
    [string]$DllDir = $env:DLL_DIR
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

Write-Host "=== atuin pwsh integration test ==="
if (-not $DllDir) {
    $DllDir = "$PSScriptRoot/../pwsh_src/AtuinNative/bin/Release/net8.0"
}
$DllDir = (Resolve-Path $DllDir).Path
Write-Host "DLL dir: $DllDir"

$asmPath = Join-Path $DllDir "AtuinNative.dll"
$manifestPath = Join-Path $DllDir "atuin-native.psd1"
if (-not (Test-Path $asmPath)) { Write-Host "FAIL: AtuinNative.dll not found"; exit 1 }
if (-not (Test-Path $manifestPath)) { Write-Host "FAIL: atuin-native.psd1 not found"; exit 1 }
Write-Host "PASS: assembly and manifest files found"

$dataDir = Join-Path ([System.IO.Path]::GetTempPath()) "atuin-pwsh-integration-$PID-$([guid]::NewGuid().ToString('N'))"
New-Item -ItemType Directory -Force -Path $dataDir | Out-Null
$env:ATUIN_DATA_DIR = $dataDir
$nativeLibName = if ($IsWindows) { 'atuin_ffi.dll' }
    elseif ($IsMacOS) { 'libatuin_ffi.dylib' }
    else { 'libatuin_ffi.so' }
$env:ATUIN_FFI_PATH = Join-Path $DllDir $nativeLibName

try {
    # ---- Direct managed-wrapper usage (Add-Type) ----
    Add-Type -Path $asmPath
    Write-Host "PASS: assembly loaded"

    $version = [AtuinNative.AtuinSession]::VersionStr()
    if ($version) { Write-Host "PASS: version $version" } else { throw "empty version" }

    [AtuinNative.AtuinEnvironment]::Set('ATUIN_NATIVE_PWSH_TEST', 'integration-test')
    if ([AtuinNative.AtuinEnvironment]::Get('ATUIN_NATIVE_PWSH_TEST') -ne 'integration-test') {
        throw "AtuinEnvironment Set/Get failed"
    }
    [AtuinNative.AtuinEnvironment]::Remove('ATUIN_NATIVE_PWSH_TEST')
    Write-Host "PASS: environment helper"

    $session = [AtuinNative.AtuinSession]::new($dataDir)
    Write-Host "PASS: session created"
    $uuid = $session.SessionUuid()
    if (-not $uuid) { throw "empty session UUID" }
    Write-Host "PASS: session UUID ($uuid)"

    $id = $session.HistoryStart("echo pwsh-integration-test", "/tmp")
    if (-not $id) { throw "HistoryStart returned empty id" }
    Write-Host "PASS: HistoryStart ($id)"

    $session.HistoryEnd($id, 0, 1000000, $true)
    Write-Host "PASS: HistoryEnd (sync)"

    $results = $session.SearchPrefix("echo pwsh-integration-test", 5)
    if ($results.Count -ne 1 -or $results[0] -ne "echo pwsh-integration-test") {
        throw "SearchPrefix returned unexpected results: $($results -join ' | ')"
    }
    Write-Host "PASS: SearchPrefix found recorded command"

    $session.Dispose()
    $session.Dispose()  # idempotent
    Write-Host "PASS: session disposed twice"

    # ---- Full module import (manifest + .psm1) ----
    Import-Module PSReadLine -ErrorAction SilentlyContinue
    Import-Module $manifestPath -Force
    Write-Host "PASS: module imported"

    $moduleSession = Get-AtuinNativeSession
    $moduleId = $moduleSession.HistoryStart("echo pwsh-module-test", "/tmp")
    $moduleSession.HistoryEnd($moduleId, 0, 0, $true)
    $moduleResults = $moduleSession.SearchPrefix("echo pwsh-module-test", 5)
    if ($moduleResults.Count -ne 1) { throw "module session search failed" }
    Write-Host "PASS: module session history/search"

    if (Get-Module PSReadLine -ErrorAction Ignore) {
        $readLineFunction = Get-Command PSConsoleHostReadLine -ErrorAction SilentlyContinue
        if ($null -eq $readLineFunction) { throw "PSConsoleHostReadLine not installed" }
        Write-Host "PASS: PSConsoleHostReadLine installed"
    } else {
        Write-Host "SKIP: PSReadLine not available"
    }

    Remove-Module atuin-native -Force
    if (Get-Module PSReadLine -ErrorAction Ignore) {
        $restored = Get-Command PSConsoleHostReadLine -ErrorAction SilentlyContinue
        if ($null -eq $restored) { throw "PSConsoleHostReadLine was not restored after module removal" }
        Write-Host "PASS: PSConsoleHostReadLine restored after module removal"
    }
    Write-Host "PASS: module removed (native session disposed by OnRemove)"

    # ---- Module unload/load cycles ----
    for ($i = 1; $i -le 3; $i++) {
        Import-Module $manifestPath -Force
        $s = Get-AtuinNativeSession
        $id2 = $s.HistoryStart("echo pwsh-cycle-$i", "/tmp")
        $s.HistoryEnd($id2, 0, 0, $true)
        Remove-Module atuin-native -Force
        Write-Host "PASS: module cycle $i"
    }

    Write-Host "=== All pwsh integration tests passed ==="
} finally {
    Remove-Module atuin-native -Force -ErrorAction SilentlyContinue
    Remove-Item -Recurse -Force $dataDir -ErrorAction SilentlyContinue
    Remove-Item Env:\ATUIN_FFI_PATH -ErrorAction SilentlyContinue
    Remove-Item Env:\ATUIN_DATA_DIR -ErrorAction SilentlyContinue
}

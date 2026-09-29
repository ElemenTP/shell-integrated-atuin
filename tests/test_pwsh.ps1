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

    # atuin_init takes no data directory: sessions resolve their
    # paths from the settings tree. Point it at the isolated temp directory so
    # the tests never read or write the developer's real ~/.config/atuin.
    $configDir = Join-Path $dataDir 'config'
    New-Item -ItemType Directory -Force -Path $configDir | Out-Null
    $dataDirToml = $dataDir.Replace('\', '/')
    Set-Content -LiteralPath (Join-Path $configDir 'config.toml') -Value "data_dir = `"$dataDirToml`""
    [AtuinNative.AtuinEnvironment]::Set('ATUIN_CONFIG_DIR', $configDir)
    [AtuinNative.AtuinEnvironment]::Set('ATUIN_DATA_DIR', $dataDir)

    $version = [AtuinNative.Session]::Version()
    if ($version) { Write-Host "PASS: version $version" } else { throw "empty version" }

    [AtuinNative.Session]::Initialize()
    Write-Host "PASS: session created"

    $firstUuid = [AtuinNative.Session]::SessionUuid()
    [AtuinNative.Session]::Initialize()   # idempotent: keeps the active session
    if ([AtuinNative.Session]::SessionUuid() -ne $firstUuid) {
        throw "second Init replaced the active session"
    }
    Write-Host "PASS: second Init is idempotent"

    $uuid = [AtuinNative.Session]::SessionUuid()
    if (-not $uuid) { throw "empty session UUID" }
    Write-Host "PASS: session UUID ($uuid)"

    $id = [AtuinNative.Session]::HistoryStart("echo pwsh-integration-test", "/tmp")
    if (-not $id) { throw "HistoryStart returned empty id" }
    Write-Host "PASS: HistoryStart ($id)"

    [AtuinNative.Session]::HistoryEnd($id, 0, 1000000, $true)
    Write-Host "PASS: HistoryEnd (sync)"

    $results = [AtuinNative.Session]::SearchPrefix("echo pwsh-integration-test", 5)
    if ($results.Count -ne 1 -or $results[0] -ne "echo pwsh-integration-test") {
        throw "SearchPrefix returned unexpected results: $($results -join ' | ')"
    }
    Write-Host "PASS: SearchPrefix found recorded command"

    $searchOptions = [AtuinNative.AtuinSearchOptions]::new()
    $searchOptions.Query = "echo pwsh-integration-test"
    $searchOptions.SearchMode = [AtuinNative.AtuinSearchMode]::Prefix
    $searchOptions.Limit = 5
    $searchOptions.Authors = @('$all-user')
    $genericResults = [AtuinNative.Session]::Search($searchOptions)
    if ($genericResults.Count -ne 1 -or $genericResults[0] -ne "echo pwsh-integration-test") {
        throw "Search returned unexpected results: $($genericResults -join ' | ')"
    }
    Write-Host "PASS: Search with upstream-compatible options found recorded command"

    [AtuinNative.Session]::Shutdown()
    [AtuinNative.Session]::Shutdown()  # idempotent
    Write-Host "PASS: session shut down twice"

    # ---- Full module import (manifest + .psm1) ----
    Import-Module PSReadLine -ErrorAction SilentlyContinue
    Import-Module $manifestPath -Force
    Write-Host "PASS: module imported"

    Initialize-AtuinNativeSession
    $moduleId = [AtuinNative.Session]::HistoryStart("echo pwsh-module-test", "/tmp")
    if (-not $moduleId) { throw "module HistoryStart returned no ID" }
    [AtuinNative.Session]::HistoryEnd($moduleId, 0, 0, $true)
    $moduleResults = [AtuinNative.Session]::SearchPrefix("echo pwsh-module-test", 5)
    if ($moduleResults.Count -ne 1) { throw "module session search failed" }
    Write-Host "PASS: module session history/search"

    $statsReport = Get-AtuinNativeStats
    if (-not $statsReport) { throw "Get-AtuinNativeStats returned nothing" }
    $stats = [AtuinNative.Session]::GetStats()
    if ($stats.HistoryStarts -lt 1 -or $stats.SearchPrefixCalls -lt 1) {
        throw "stats did not count the module session's operations: $statsReport"
    }
    Write-Host "PASS: session stats ($statsReport)"

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

        $ctrlRHandler = Get-PSReadLineKeyHandler -Chord 'Ctrl+r' | Select-Object -First 1
        $ctrlR = if ($null -ne $ctrlRHandler) { $ctrlRHandler.Function } else { $null }
        if ($ctrlR -ne 'ReverseSearchHistory') {
            throw "Ctrl+r was not restored after module removal (got '$ctrlR')"
        }
        $upArrowHandler = Get-PSReadLineKeyHandler -Chord 'UpArrow' | Select-Object -First 1
        $upArrow = if ($null -ne $upArrowHandler) { $upArrowHandler.Function } else { $null }
        if ($upArrow -ne 'PreviousHistory') {
            throw "UpArrow was not restored after module removal (got '$upArrow')"
        }
        Write-Host "PASS: Ctrl+r / UpArrow key handlers restored after module removal"
    }

    # OnRemove must dispose the native session, not just unbind the module.
    $sessionGone = $false
    try { [AtuinNative.Session]::GetStats() | Out-Null } catch { $sessionGone = $true }
    if (-not $sessionGone) { throw "native session still alive after module removal" }
    Write-Host "PASS: native session disposed by OnRemove"

    # ---- Module unload/load cycles ----
    for ($i = 1; $i -le 3; $i++) {
        Import-Module $manifestPath -Force
        Initialize-AtuinNativeSession
        $id2 = [AtuinNative.Session]::HistoryStart("echo pwsh-cycle-$i", "/tmp")
        if (-not $id2) { throw "cycle ${i}: HistoryStart returned no ID" }
        [AtuinNative.Session]::HistoryEnd($id2, 0, 0, $true)
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

#Requires -Version 7.2
<#
.SYNOPSIS
    atuin-native — in-process Atuin shell history for PowerShell (pwsh 7+).

.DESCRIPTION
    This module mirrors the official atuin/src/shell/atuin.ps1 integration,
    replacing every `atuin` process spawn with the in-process
    [AtuinNative.AtuinSession] managed wrapper (see AtuinNative.dll).

    The native library (libatuin_ffi.so / libatuin_ffi.dylib /
    atuin_ffi.dll) must be present next to AtuinNative.dll. To load it from
    another location, set the ATUIN_FFI_PATH environment variable.

.EXAMPLE
    Import-Module atuin-native
    Enable-AtuinSearchKeys
#>

Set-StrictMode -Version Latest

# ---- Ensure the binary assembly is available --------------------------------
# Normally it is loaded as a NestedModule from the manifest; this fallback only
# matters when the .psm1 is imported directly during development.
if (-not ('AtuinNative.AtuinSession' -as [type])) {
    Import-Module (Join-Path $PSScriptRoot 'AtuinNative.dll') -ErrorAction Stop
}

# ---- Point the FFI resolver at a colocated native library -------------------
$script:NativeLibName = switch ($true) {
    $IsWindows { 'atuin_ffi.dll' }
    $IsMacOS   { 'libatuin_ffi.dylib' }
    default    { 'libatuin_ffi.so' }
}
if (-not $env:ATUIN_FFI_PATH) {
    $colocated = Join-Path $PSScriptRoot $script:NativeLibName
    if (Test-Path -LiteralPath $colocated -PathType Leaf) {
        $env:ATUIN_FFI_PATH = $colocated
    }
}

# ---- Environment visible to native Rust code ---------------------------------
# .NET's $env: does not call setenv() on Unix; the embedded Rust library reads
# ATUIN_SHELL / ATUIN_SESSION through std::env, so write both blocks.
[AtuinNative.AtuinEnvironment]::Set('ATUIN_SHELL', 'pwsh')

# ---- Native session (created once, lives for the pwsh process) ---------------
$script:Session = $null
$script:NativeWarned = $false
# Used by the global PSConsoleHostReadLine wrapper to invoke module-private
# functions even though global functions execute in the global session state.
$script:AtuinNativeModule = $ExecutionContext.SessionState.Module

function Get-AtuinNativeSession {
    <#
    .SYNOPSIS
        Returns the process-wide in-process Atuin session, creating it on first
        use.
    #>
    if ($null -eq $script:Session) {
        $dataDir = $env:ATUIN_DATA_DIR
        $script:Session = [AtuinNative.AtuinSession]::new($dataDir)

        $uuid = $script:Session.SessionUuid()
        $env:ATUIN_SESSION = $uuid
        [AtuinNative.AtuinEnvironment]::Set('ATUIN_SESSION', $uuid)
        $env:ATUIN_PID = $PID
    }
    return $script:Session
}

function Get-AtuinNativeVersion {
    <#
    .SYNOPSIS
        Returns the version of the embedded atuin-ffi native library.
    #>
    return [AtuinNative.AtuinSession]::VersionStr()
}

# ---- PSReadLine integration --------------------------------------------------
# The official atuin.ps1 replaces PSConsoleHostReadLine so that
#   * the previous command is finalized before PSReadLine reads the next one
#   * the next command is recorded after it has been read
# The same logic is kept here, but HistoryStart/HistoryEnd are direct calls.

$script:AtuinHistoryId = $null
$script:PreviousPSConsoleHostReadLine = $null
$script:HasExpectedReadLineOverload = $false

function Get-AtuinCommandLine {
    $line = $null
    try {
        [Microsoft.PowerShell.PSConsoleReadLine]::GetBufferState([ref]$line, [ref]$null)
    } catch {}
    return $line
}

function Set-AtuinCommandLine {
    param([AllowNull()][string]$Text)

    if ($null -eq $Text) { $Text = "" }
    $commandLine = Get-AtuinCommandLine
    if ($null -ne $commandLine) {
        [Microsoft.PowerShell.PSConsoleReadLine]::Replace(0, $commandLine.Length, $Text)
    }
}

function Invoke-AtuinNativeReadLine {
    # 1. Collect the exit code of the previous command.
    $lastRunStatus = $?
    $lastNativeExitCode = $global:LASTEXITCODE
    $exitCode = if ($lastRunStatus) { 0 } elseif ($lastNativeExitCode) { $lastNativeExitCode } else { 1 }

    # 2. Report the status of the previous command to Atuin.
    if ($script:AtuinHistoryId) {
        try {
            $duration = 0
            $lastHistory = Get-History -Count 1 -ErrorAction SilentlyContinue
            if ($null -ne $lastHistory) {
                # .NET TimeSpan.Ticks are 100 ns; Atuin stores nanoseconds.
                $duration = [long]($lastHistory.Duration.Ticks * 100)
            }
            (Get-AtuinNativeSession).HistoryEnd($script:AtuinHistoryId, [long]$exitCode, $duration, $false)
        } catch {
            # Ignore errors so shell input is never blocked by history failure.
        } finally {
            $script:AtuinHistoryId = $null
        }
    }

    # 3. Read the next command line.
    Microsoft.PowerShell.Core\Set-StrictMode -Off
    $line = if ($script:HasExpectedReadLineOverload) {
        [Microsoft.PowerShell.PSConsoleReadLine]::ReadLine(
            $Host.Runspace,
            $ExecutionContext,
            [System.Threading.CancellationToken]::None,
            $lastRunStatus)
    } else {
        if ($null -ne $script:PreviousPSConsoleHostReadLine) {
            & $script:PreviousPSConsoleHostReadLine
        } else {
            [Microsoft.PowerShell.PSConsoleReadLine]::ReadLine()
        }
    }
    Microsoft.PowerShell.Core\Set-StrictMode -Version Latest

    # 4. Report the next command line to Atuin.
    if (-not [string]::IsNullOrEmpty($line)) {
        try {
            $cwd = if ($ExecutionContext.SessionState.Path.CurrentLocation.Provider.Name -eq 'FileSystem') {
                $ExecutionContext.SessionState.Path.CurrentLocation.ProviderPath
            } else {
                (Get-Location).Path
            }
            $script:AtuinHistoryId = (Get-AtuinNativeSession).HistoryStart($line, $cwd)
        } catch {
            # Ignore errors to avoid breaking the shell.
        }
    }

    $global:LASTEXITCODE = $lastNativeExitCode
    return $line
}

if (Get-Module PSReadLine -ErrorAction Ignore) {
    $script:HasExpectedReadLineOverload = (
        [Microsoft.PowerShell.PSConsoleReadLine]::ReadLine
    ).OverloadDefinitions.Contains(
        "static string ReadLine(runspace runspace, System.Management.Automation.EngineIntrinsics engineIntrinsics, System.Threading.CancellationToken cancellationToken, System.Nullable[bool] lastRunStatus)")

    if (Get-Command PSConsoleHostReadLine -ErrorAction Ignore) {
        $script:PreviousPSConsoleHostReadLine = $Function:PSConsoleHostReadLine
    }

    function global:PSConsoleHostReadLine {
        & $script:AtuinNativeModule { Invoke-AtuinNativeReadLine }
    }
} else {
    Write-Warning "atuin-native: PSReadLine is not loaded; history recording and search keys are disabled."
}

# ---- Interactive search (native prefix search) ------------------------------
# The official search spawns the Atuin TUI. This in-process variant returns
# prefix matches and replaces the command line with the first match.
function Invoke-AtuinSearch {
    <#
    .SYNOPSIS
        Replaces the current command line with the newest prefix match from
        Atuin history.
    #>
    [CmdletBinding()]
    param([string]$ExtraArgs = "")

    if (-not (Get-Module PSReadLine -ErrorAction Ignore)) {
        Write-Warning "atuin-native: PSReadLine is required for Invoke-AtuinSearch."
        return
    }

    $query = Get-AtuinCommandLine
    if ([string]::IsNullOrEmpty($query)) { return }

    try {
        $results = (Get-AtuinNativeSession).SearchPrefix($query, 20)
    } catch {
        if (-not $script:NativeWarned) {
            $script:NativeWarned = $true
            Write-Warning "atuin-native: search failed. $_"
        }
        return
    }

    if ($results.Count -eq 0) { return }
    Set-AtuinCommandLine $results[0]
}

function Enable-AtuinSearchKeys {
    <#
    .SYNOPSIS
        Binds Ctrl+R and UpArrow to the in-process Atuin history search.
    #>
    param([bool]$CtrlR = $true, [bool]$UpArrow = $true)

    if (-not (Get-Module PSReadLine -ErrorAction Ignore)) {
        Write-Warning "atuin-native: PSReadLine is required for search keys."
        return
    }

    if ($CtrlR) {
        Set-PSReadLineKeyHandler -Chord "Ctrl+r" -BriefDescription "Runs Atuin native search" -ScriptBlock {
            Invoke-AtuinSearch
        }
    }

    if ($UpArrow) {
        Set-PSReadLineKeyHandler -Chord "UpArrow" -BriefDescription "Runs Atuin native search" -ScriptBlock {
            $line = Get-AtuinCommandLine
            if ($null -ne $line -and -not $line.Contains("`n")) {
                Invoke-AtuinSearch -ExtraArgs "--shell-up-key-binding"
            } else {
                [Microsoft.PowerShell.PSConsoleReadLine]::PreviousLine()
            }
        }
    }
}

# ---- Export the public API ---------------------------------------------------
Export-ModuleMember -Function @(
    "Get-AtuinNativeSession"
    "Get-AtuinNativeVersion"
    "Invoke-AtuinSearch"
    "Enable-AtuinSearchKeys"
    "PSConsoleHostReadLine"
)

# ---- Restore PSReadLine and dispose the native session on module removal -----
$MyInvocation.MyCommand.ScriptBlock.Module.OnRemove = {
    if ($null -ne $script:PreviousPSConsoleHostReadLine) {
        Set-Item -Path function:\PSConsoleHostReadLine -Value $script:PreviousPSConsoleHostReadLine
    } else {
        Remove-Item -Path function:\PSConsoleHostReadLine -ErrorAction Ignore
    }

    $env:ATUIN_SESSION = $null
    $env:ATUIN_PID = $null

    if ($null -ne $script:Session) {
        try { $script:Session.Dispose() } catch {}
        $script:Session = $null
    }
}

#Requires -Version 7.2
<#
.SYNOPSIS
    atuin-native — in-process Atuin shell history for PowerShell (pwsh 7+).

.DESCRIPTION
    This module mirrors the official atuin/src/shell/atuin.ps1 integration,
    replacing every `atuin` process spawn with the in-process
    [AtuinNative.Session] managed wrapper (see AtuinNative.dll).

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
if (-not ('AtuinNative.Session' -as [type])) {
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
[AtuinNative.AtuinEnvironment]::Set('ATUIN_SHELL', 'powershell')

# ---- Native session (one process-wide session, created on first use) ---------
$script:SessionInitialized = $false
$script:NativeWarned = $false
# Used by the global PSConsoleHostReadLine wrapper to invoke module-private
# functions even though global functions execute in the global session state.
$script:AtuinNativeModule = $ExecutionContext.SessionState.Module

function Initialize-AtuinNativeSession {
    <#
    .SYNOPSIS
        Ensures the process-wide in-process Atuin session exists.
    #>
    if (-not $script:SessionInitialized) {
        # No data directory is passed: the native session resolves it from the
        # user's Atuin configuration exactly like the official CLI.
        [AtuinNative.Session]::Initialize()

        $uuid = [AtuinNative.Session]::SessionUuid()
        $env:ATUIN_SESSION = $uuid
        [AtuinNative.AtuinEnvironment]::Set('ATUIN_SESSION', $uuid)
        $env:ATUIN_PID = $PID
        $script:SessionInitialized = $true
    }
}

function Get-AtuinNativeVersion {
    <#
    .SYNOPSIS
        Returns the version of the embedded atuin-ffi native library.
    #>
    return [AtuinNative.Session]::Version()
}

function Get-AtuinNativeStats {
    <#
    .SYNOPSIS
        Returns a summary of the in-process session's operation counters.
    #>
    Initialize-AtuinNativeSession
    return [AtuinNative.Session]::GetStatsReport()
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

function Reset-AtuinPrompt {
    <#
    .SYNOPSIS
        Reset PSReadLine's cursor state after the in-process TUI repainted.

    .DESCRIPTION
        PSReadLine maintains its own cursor position, which is no longer valid
        once the search TUI has scrolled the display. `InvokePrompt` accepts a
        new Y position and rebuilds that state. This mirrors the official
        atuin.ps1, including the ATUIN_POWERSHELL_PROMPT_OFFSET contract:
        when unset it is derived from the current prompt's line count (so a
        multi-line prompt offsets correctly) and users can override it.
    #>
    if ($null -eq $env:ATUIN_POWERSHELL_PROMPT_OFFSET) {
        try {
            $promptLines = (& $Function:prompt | Out-String | Measure-Object -Line).Lines
            $env:ATUIN_POWERSHELL_PROMPT_OFFSET = -1 * ($promptLines - 1)
        } catch {
            $env:ATUIN_POWERSHELL_PROMPT_OFFSET = 0
        }
    }

    try {
        $y = $Host.UI.RawUI.CursorPosition.Y + [int]$env:ATUIN_POWERSHELL_PROMPT_OFFSET
        $y = [System.Math]::Max([System.Math]::Min($y, [System.Console]::BufferHeight - 1), 0)
        [Microsoft.PowerShell.PSConsoleReadLine]::InvokePrompt($null, $y)
    } catch {
        # Refreshing the prompt is best-effort; never break the shell over it.
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
            Initialize-AtuinNativeSession
            [AtuinNative.Session]::HistoryEnd($script:AtuinHistoryId, [long]$exitCode, $duration, $false)
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
            Initialize-AtuinNativeSession
            $script:AtuinHistoryId = [AtuinNative.Session]::HistoryStart($line, $cwd)
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

# ---- Interactive search (in-process full-screen TUI) ------------------------
# The official atuin.ps1 launches `atuin search -i` as a child process. This
# variant calls the same TUI implementation directly inside the pwsh process
# through the native session, so no external atuin binary is required.
function Invoke-AtuinSearch {
    <#
    .SYNOPSIS
        Opens the in-process Atuin interactive search TUI and replaces the
        current command line with the selection.

    .DESCRIPTION
        The current PSReadLine buffer is used as the initial query. Pass
        -ShellUpKeyBinding for the UpArrow widget and -KeymapMode for the vi
        widgets; the official PowerShell module forwards the equivalent
        `atuin search -i --shell-up-key-binding --keymap-mode=…` flags, but
        in-process these are ordinary typed parameters.

    .PARAMETER ShellUpKeyBinding
        Mirror `atuin search -i --shell-up-key-binding` (the shell's UpArrow
        widget): UpArrow inside the TUI walks history instead of moving within
        the command line.

    .PARAMETER KeymapMode
        Mirror `atuin search -i --keymap-mode`: the vi widgets pass VimNormal /
        VimInsert so the TUI starts in the matching keymap.
    #>
    [CmdletBinding()]
    param(
        [switch]$ShellUpKeyBinding,
        [AtuinNative.AtuinKeymapMode]$KeymapMode = [AtuinNative.AtuinKeymapMode]::Auto
    )

    if (-not (Get-Module PSReadLine -ErrorAction Ignore)) {
        Write-Warning "atuin-native: PSReadLine is required for Invoke-AtuinSearch."
        return
    }

    $query = Get-AtuinCommandLine

    try {
        Initialize-AtuinNativeSession
        $selected = [AtuinNative.Session]::SearchInteractive(
            $query, [bool]$ShellUpKeyBinding, $KeymapMode)
    } catch {
        if (-not $script:NativeWarned) {
            $script:NativeWarned = $true
            Write-Warning "atuin-native: search failed. $_"
        }
        return
    }

    # The TUI repainted the screen, so PSReadLine's cached cursor state is stale.
    Reset-AtuinPrompt

    if ($null -eq $selected) {
        # Esc / Ctrl+C / Ctrl+G: keep the buffer exactly as it was.
        return
    }

    $acceptPrefix = "__atuin_accept__:"
    if ($selected.StartsWith($acceptPrefix, [System.StringComparison]::Ordinal)) {
        Set-AtuinCommandLine $selected.Substring($acceptPrefix.Length)
        [Microsoft.PowerShell.PSConsoleReadLine]::AcceptLine()
    } else {
        Set-AtuinCommandLine $selected
    }
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
                Invoke-AtuinSearch -ShellUpKeyBinding
            } else {
                [Microsoft.PowerShell.PSConsoleReadLine]::PreviousLine()
            }
        }
    }
}

# ---- Bind search keys by default, exactly like the original module -----------
# `atuin init powershell` ends with `Enable-AtuinSearchKeys -CtrlR $true
# -UpArrow $true`; without this, importing the module defines the handler but
# Ctrl+R / UpArrow keep their PSReadLine defaults and never reach the TUI.
if (Get-Module PSReadLine -ErrorAction Ignore) {
    Enable-AtuinSearchKeys -CtrlR $true -UpArrow $true
}

# ---- Export the public API ---------------------------------------------------
Export-ModuleMember -Function @(
    "Initialize-AtuinNativeSession"
    "Get-AtuinNativeVersion"
    "Get-AtuinNativeStats"
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

    if ($script:SessionInitialized) {
        try { [AtuinNative.Session]::Shutdown() } catch {}
        $script:SessionInitialized = $false
    }
}

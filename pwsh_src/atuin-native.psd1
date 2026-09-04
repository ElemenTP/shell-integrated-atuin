@{
    # atuin-native module manifest.
    #
    # Hybrid script + binary module: the .psm1 contains the shell integration
    # and is the RootModule; the C# assembly (AtuinNative.dll) is loaded as a
    # NestedModule so its types are available to the .psm1 at import time.

    RootModule           = 'atuin-native.psm1'
    ModuleVersion        = '0.1.0'
    GUID                 = '0604edc0-e2e5-4069-9acc-1e18b51c4644'

    Author               = 'Atuin Contributors'
    CompanyName          = 'Atuin Contributors'
    Copyright            = '(c) Atuin Contributors. MIT license.'

    Description          = 'In-process Atuin shell history for PowerShell (pwsh 7+). Records history and searches inside the shell process via a Rust FFI library — zero fork/exec per command.'

    PowerShellVersion    = '7.2'
    CompatiblePSEditions = @('Core')

    NestedModules        = @('AtuinNative.dll')

    # PSConsoleHostReadLine is installed as a global function by the .psm1 and
    # is exported here for compatibility with the official atuin module.
    FunctionsToExport    = @(
        'Get-AtuinNativeSession'
        'Get-AtuinNativeVersion'
        'Invoke-AtuinSearch'
        'Enable-AtuinSearchKeys'
        'PSConsoleHostReadLine'
    )
    CmdletsToExport      = @()
    VariablesToExport    = @()
    AliasesToExport      = @()

    PrivateData          = @{
        PSData = @{
            Tags = @(
                'atuin', 'history', 'shell', 'PSReadLine',
                'PSEdition_Core', 'Linux', 'macOS', 'Windows',
                'native', 'in-process', 'rust', 'ffi'
            )
            ProjectUri              = 'https://github.com/ElemenTP/atuin'
            LicenseUri              = 'https://github.com/ElemenTP/atuin/blob/main/LICENSE'
            IconUri                 = ''
            ReleaseNotes            = @'
## 0.1.0

- Initial release: in-process Atuin shell history for PowerShell (pwsh 7+).
- Zero-fork history recording and prefix search through a Rust FFI library.
- Persistent session: SQLite connections, settings and encryption key are reused across commands.
- `Enable-AtuinSearchKeys` binds Ctrl+R / UpArrow to in-process search.
'@
            Prerelease              = ''
            RequireLicenseAcceptance = $false
        }
    }
}

using System.Reflection;
using System.Runtime.InteropServices;

namespace AtuinNative;

/// <summary>
/// P/Invoke declarations for the atuin-ffi native library.
///
/// Uses .NET 7+ LibraryImport source generators for compile-time stub
/// generation — faster invocation and AOT-friendly compared to DllImport.
///
/// On Linux/macOS the native library (libatuin_ffi.so / .dylib) must be
/// placed alongside AtuinNative.dll. On Windows, atuin_ffi.dll must be in the
/// same directory.
///
/// A custom <see cref="NativeLibrary.SetDllImportResolver"/> honors the
/// ATUIN_FFI_PATH environment variable (absolute path to the native lib),
/// falling back to default .NET resolution (which probes the directory of
/// this assembly — exactly where the CMake build copies the library).
///
/// # Error protocol
/// Every fallible export returns an error pointer: <see cref="IntPtr.Zero"/>
/// means success, otherwise the pointer is an allocated UTF-8 error string
/// that the caller must free with <see cref="Free"/>. Values produced by a
/// call are written through out parameters.
///
/// # Single session
/// The native library exposes at most one process-wide session (Atuin's client
/// layer keeps process-global state), so no export takes a session handle.
/// </summary>
public enum AtuinSearchMode
{
    Auto = 0,
    Prefix = 1,
    FullText = 2,
    Fuzzy = 3,
    DaemonFuzzy = 4,
}

public enum AtuinFilterMode
{
    Auto = 0,
    Global = 1,
    Host = 2,
    Session = 3,
    Directory = 4,
    Workspace = 5,
    SessionPreload = 6,
}

/// <summary>Interactive TUI keymap mode (mirrors <c>atuin search --keymap-mode</c>).</summary>
public enum AtuinKeymapMode
{
    Auto = 0,
    Emacs = 1,
    VimNormal = 2,
    VimInsert = 3,
}

/// <summary>Whether a command was run by a human or an agent (mirrors <c>atuin history start --author-kind</c>).</summary>
public enum AtuinAuthorKind
{
    User,
    Agent,
}

/// <summary>C-compatible snapshot of the session's operation counters.</summary>
[StructLayout(LayoutKind.Sequential)]
public struct AtuinStats
{
    public ulong HistoryStarts;
    public ulong HistoryEndsSync;
    public ulong HistoryEndsAsync;
    public ulong SearchCalls;
    public ulong SearchPrefixCalls;
    public ulong InteractiveSearchCalls;
    public ulong InteractiveSelections;
    public ulong InteractiveCancels;
    public ulong InFlightHistoryEnds;
    public ulong UptimeSecs;
}

[StructLayout(LayoutKind.Sequential)]
internal struct AtuinSearchOptionsNative
{
    public IntPtr Query;
    public int SearchMode;
    public int FilterMode;
    public IntPtr Cwd;
    public IntPtr ExcludeCwd;
    /// <summary>Pointer to <see cref="ExitCount"/> 64-bit exit codes.</summary>
    public IntPtr Exits;
    public UIntPtr ExitCount;
    /// <summary>Pointer to <see cref="ExcludeExitCount"/> 64-bit exit codes.</summary>
    public IntPtr ExcludeExits;
    public UIntPtr ExcludeExitCount;
    public IntPtr Before;
    public IntPtr After;
    public int HasLimit;
    public long Limit;
    public int HasOffset;
    public long Offset;
    public int Reverse;
    public int IncludeDuplicates;
    public IntPtr Authors;
    public UIntPtr AuthorCount;
    public IntPtr Shells;
    public UIntPtr ShellCount;
}

internal static unsafe partial class NativeMethods
{
    // Platform-specific library name. .NET runtime resolves these as:
    //   Linux:   libatuin_ffi.so
    //   macOS:   libatuin_ffi.dylib
    //   Windows: atuin_ffi.dll
    private const string LibName = "atuin_ffi";

    static NativeMethods()
    {
        NativeLibrary.SetDllImportResolver(typeof(NativeMethods).Assembly, ResolveNativeLibrary);
    }

    /// <summary>
    /// Resolve the atuin-ffi native library. Honors ATUIN_FFI_PATH; otherwise
    /// defers to the default runtime resolution so the library is found next
    /// to AtuinNative.dll.
    /// </summary>
    private static IntPtr ResolveNativeLibrary(
        string libraryName, Assembly assembly, DllImportSearchPath? searchPath)
    {
        if (!string.Equals(libraryName, LibName, StringComparison.OrdinalIgnoreCase))
            return IntPtr.Zero;

        string? overridePath = Environment.GetEnvironmentVariable("ATUIN_FFI_PATH");
        if (!string.IsNullOrEmpty(overridePath))
        {
            string fullPath = Path.GetFullPath(overridePath);
            if (File.Exists(fullPath))
            {
                return NativeLibrary.Load(fullPath);
            }
        }

        // Fall back to default resolution (probes the assembly directory).
        return IntPtr.Zero;
    }

    // ── Session lifecycle ──────────────────────────────────────────────

    /// <summary>
    /// Create the process-wide history session using the data directory
    /// resolved from the user's Atuin configuration (ATUIN_DATA_DIR, XDG, or
    /// config.toml), exactly like the official CLI. Returns
    /// <see cref="IntPtr.Zero"/> on success (including when a session is
    /// already active), or an allocated error string when creation failed.
    /// </summary>
    [LibraryImport(LibName, EntryPoint = "atuin_init")]
    internal static partial IntPtr Init();

    /// <summary>
    /// Destroy the process-wide session. Calling it with no active session is a
    /// successful no-op, so it is safe on cleanup.
    /// </summary>
    [LibraryImport(LibName, EntryPoint = "atuin_shutdown")]
    internal static partial IntPtr Shutdown();

    // ── History recording ──────────────────────────────────────────────

    /// <summary>
    /// Record a command start. On success returns <see cref="IntPtr.Zero"/> and
    /// writes a Rust-allocated history ID to <paramref name="idOut"/>; the
    /// caller must free it with <see cref="Free"/>.
    /// <paramref name="authorKind"/> is <c>"user"</c> or <c>"agent"</c>.
    /// </summary>
    [LibraryImport(LibName, EntryPoint = "atuin_history_start", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial IntPtr HistoryStart(
        string command, string cwd,
        string? author, string? authorKind, string? intent, out IntPtr idOut);

    /// <summary>
    /// Finalize a command. When <paramref name="sync"/> is non-zero, the call
    /// blocks and returns an error string on failure; otherwise it schedules
    /// the update and returns immediately.
    /// </summary>
    [LibraryImport(LibName, EntryPoint = "atuin_history_end", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial IntPtr HistoryEnd(
        string id, long exitCode, long durationNs, int sync);

    // ── Search ─────────────────────────────────────────────────────────

    /// <summary>
    /// Generic search with upstream-compatible options. On success writes a
    /// newline-separated UTF-8 string of command texts to
    /// <paramref name="output"/>; the caller must free it with <see cref="Free"/>.
    /// </summary>
    [LibraryImport(LibName, EntryPoint = "atuin_search")]
    internal static partial IntPtr Search(
        in AtuinSearchOptionsNative options, out IntPtr output);

    /// <summary>
    /// Prefix search. On success writes a newline-separated UTF-8 string to
    /// <paramref name="output"/>; the caller must free it with <see cref="Free"/>.
    /// </summary>
    [LibraryImport(LibName, EntryPoint = "atuin_search_prefix", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial IntPtr SearchPrefix(
        string? query, int limit, out IntPtr output);

    /// <summary>
    /// Interactive full-screen search TUI. On success returns
    /// <see cref="IntPtr.Zero"/>; <paramref name="output"/> is non-zero when the
    /// user selected a command (free it with <see cref="Free"/>) and zero when
    /// the user cancelled. Blocks until the user selects a command or cancels.
    /// <paramref name="shellUpKeyBinding"/> and <paramref name="keymapMode"/>
    /// mirror the official <c>--shell-up-key-binding</c> / <c>--keymap-mode</c>.
    /// </summary>
    [LibraryImport(LibName, EntryPoint = "atuin_search_interactive", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial IntPtr SearchInteractive(
        string? query, int shellUpKeyBinding, int keymapMode, out IntPtr output);

    /// <summary>
    /// Write a snapshot of the session's operation counters to
    /// <paramref name="stats"/>. Returns <see cref="IntPtr.Zero"/> on success.
    /// </summary>
    [LibraryImport(LibName, EntryPoint = "atuin_stats")]
    internal static partial IntPtr Stats(out AtuinStats stats);

    // ── Memory management ──────────────────────────────────────────────

    /// <summary>
    /// Free a string returned by a fallible export (history ID, search result,
    /// selected command, or error string). NULL-safe.
    /// </summary>
    [LibraryImport(LibName, EntryPoint = "atuin_free")]
    internal static partial void Free(IntPtr ptr);

    // ── Metadata ───────────────────────────────────────────────────────

    /// <summary>
    /// Write the session UUID to <paramref name="uuid"/> (session-owned, must
    /// NOT be freed). Returns <see cref="IntPtr.Zero"/> on success.
    /// </summary>
    [LibraryImport(LibName, EntryPoint = "atuin_session_uuid")]
    internal static partial IntPtr SessionUuid(out IntPtr uuid);

    /// <summary>Return the library version (static, must NOT be freed).</summary>
    [LibraryImport(LibName, EntryPoint = "atuin_version")]
    internal static partial IntPtr Version();
}

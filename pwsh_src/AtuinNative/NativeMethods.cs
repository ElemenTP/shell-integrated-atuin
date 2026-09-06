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
/// Strings returned by atuin_history_start and atuin_search_prefix must be
/// freed with atuin_free_string. atuin_session_uuid / atuin_version /
/// atuin_last_error return library-owned pointers and must NOT be freed.
/// </summary>
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

    /// <summary>Create a new history session. Returns <see cref="IntPtr.Zero"/> on failure.</summary>
    [LibraryImport(LibName, EntryPoint = "atuin_session_create", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial IntPtr SessionCreate(string? dataDir);

    /// <summary>Destroy a session. Passing <see cref="IntPtr.Zero"/> is safe (no-op).</summary>
    [LibraryImport(LibName, EntryPoint = "atuin_session_destroy")]
    internal static partial void SessionDestroy(IntPtr session);

    // ── History recording ──────────────────────────────────────────────

    /// <summary>
    /// Record a command start. On success (return 0), writes a Rust-allocated
    /// history ID to <paramref name="idOut"/>; the caller must free it with
    /// <see cref="FreeString"/>. On failure, the output slot is reset to NULL.
    /// </summary>
    [LibraryImport(LibName, EntryPoint = "atuin_history_start", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int HistoryStart(
        IntPtr session, string command, string cwd,
        string? author, string? intent, out IntPtr idOut);

    /// <summary>
    /// Finalize a command. When <paramref name="sync"/> is non-zero, the call
    /// blocks and returns 0 on success; otherwise it schedules the update and
    /// returns immediately.
    /// </summary>
    [LibraryImport(LibName, EntryPoint = "atuin_history_end", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int HistoryEnd(
        IntPtr session, string id, long exitCode, long durationNs, int sync);

    // ── Search ─────────────────────────────────────────────────────────

    /// <summary>
    /// Prefix search. On success writes a newline-separated UTF-8 string to
    /// <paramref name="out"/>; the caller must free it with
    /// <see cref="FreeString"/>.
    /// </summary>
    [LibraryImport(LibName, EntryPoint = "atuin_search_prefix", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int SearchPrefix(
        IntPtr session, string? query, int limit, out IntPtr @out);

    /// <summary>
    /// Interactive full-screen search TUI. Return 0 = selected (free the
    /// output with <see cref="FreeString"/>), 1 = cancelled, negative = error.
    /// Blocks until the user selects a command or cancels.
    /// </summary>
    [LibraryImport(LibName, EntryPoint = "atuin_search_interactive", StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int SearchInteractive(
        IntPtr session, string? query, out IntPtr @out);

    // ── Memory management ──────────────────────────────────────────────

    /// <summary>Free a string returned by history_start or search_prefix. NULL-safe.</summary>
    [LibraryImport(LibName, EntryPoint = "atuin_free_string")]
    internal static partial void FreeString(IntPtr ptr);

    // ── Metadata ───────────────────────────────────────────────────────

    /// <summary>Return the session UUID (session-owned, must NOT be freed).</summary>
    [LibraryImport(LibName, EntryPoint = "atuin_session_uuid")]
    internal static partial IntPtr SessionUuid(IntPtr session);

    /// <summary>Return the library version (static, must NOT be freed).</summary>
    [LibraryImport(LibName, EntryPoint = "atuin_version")]
    internal static partial IntPtr Version();

    /// <summary>
    /// Return the last error pointer (library-owned, valid until the next FFI
    /// call). Returns <see cref="IntPtr.Zero"/> if no error is set.
    /// </summary>
    [LibraryImport(LibName, EntryPoint = "atuin_last_error")]
    internal static partial IntPtr LastError();
}

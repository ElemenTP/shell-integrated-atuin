using System.Runtime.InteropServices;
using System.Collections.Generic;

namespace AtuinNative;

/// <summary>
/// Options for a non-interactive history search. Mirrors the upstream
/// `atuin search` filtering and search-mode flags.
/// </summary>
public sealed class AtuinSearchOptions
{
    public string? Query { get; set; }
    public AtuinSearchMode SearchMode { get; set; } = AtuinSearchMode.Auto;
    public AtuinFilterMode FilterMode { get; set; } = AtuinFilterMode.Auto;
    public string? Cwd { get; set; }
    public string? ExcludeCwd { get; set; }
    /// <summary>Include only these exit codes (mirrors the repeatable <c>--exit</c>).</summary>
    public long[]? Exits { get; set; }
    /// <summary>Exclude these exit codes (mirrors the repeatable <c>--exclude-exit</c>).</summary>
    public long[]? ExcludeExits { get; set; }
    public string? Before { get; set; }
    public string? After { get; set; }
    public long? Limit { get; set; }
    public long? Offset { get; set; }
    public bool Reverse { get; set; }
    public bool IncludeDuplicates { get; set; }
    public string[]? Authors { get; set; }
    public string[]? Shells { get; set; }
}

/// <summary>
/// Managed wrapper around the atuin-ffi native library.
///
/// The native library exposes exactly one process-wide session (Atuin's client
/// layer keeps process-global state such as the resolved data directory and the
/// meta store), so this type is static: call <see cref="Init"/> once and
/// <see cref="Shutdown"/> before unloading.
///
/// Every native call returns an error pointer (NULL = success). This wrapper
/// converts a non-NULL error into an <see cref="InvalidOperationException"/>
/// and frees the native string.
/// </summary>
public static class Session
{
    /// <summary>
    /// Create the process-wide history session. The Atuin data directory is
    /// resolved from the user's configuration (ATUIN_DATA_DIR, XDG, or
    /// config.toml), exactly like the official CLI. Calling it while a session
    /// is already active is an idempotent no-op that keeps that session.
    /// </summary>
    /// <exception cref="InvalidOperationException">Creation failed.</exception>
    public static void Initialize()
    {
        IntPtr error = NativeMethods.Init();
        if (error != IntPtr.Zero)
        {
            throw new InvalidOperationException(
                $"Failed to initialize atuin session: {TakeError(error)}");
        }
    }

    /// <summary>
    /// Destroy the process-wide history session. Calling it when no session is
    /// active is a successful no-op, so it is safe to call on cleanup.
    /// </summary>
    public static void Shutdown()
    {
        IntPtr error = NativeMethods.Shutdown();
        if (error != IntPtr.Zero)
        {
            throw new InvalidOperationException(
                $"Failed to shutdown atuin session: {TakeError(error)}");
        }
    }

    /// <summary>
    /// Record a command start and return the history ID. The optional
    /// <paramref name="author"/> / <paramref name="authorKind"/> /
    /// <paramref name="intent"/> mirror
    /// <c>atuin history start --author / --author-kind / --intent</c>.
    /// </summary>
    public static string HistoryStart(
        string command,
        string cwd = "",
        string? author = null,
        AtuinAuthorKind? authorKind = null,
        string? intent = null)
    {
        IntPtr error = NativeMethods.HistoryStart(
            command, cwd ?? string.Empty, author, AuthorKindToString(authorKind),
            intent, out IntPtr idPtr);
        if (error != IntPtr.Zero)
        {
            throw new InvalidOperationException(
                $"atuin HistoryStart failed: {TakeError(error)}");
        }
        if (idPtr == IntPtr.Zero)
        {
            throw new InvalidOperationException(
                "atuin HistoryStart succeeded but produced no history ID");
        }

        try
        {
            return Marshal.PtrToStringUTF8(idPtr) ?? string.Empty;
        }
        finally
        {
            NativeMethods.Free(idPtr);
        }
    }

    /// <summary>Map <see cref="AtuinAuthorKind"/> to the CLI spelling.</summary>
    private static string? AuthorKindToString(AtuinAuthorKind? kind) => kind switch
    {
        AtuinAuthorKind.User => "user",
        AtuinAuthorKind.Agent => "agent",
        _ => null,
    };

    /// <summary>
    /// Finalize a command.
    /// </summary>
    /// <param name="sync">When true, block until the database update completes
    /// and throw on failure. When false, schedule fire-and-forget work like
    /// the official `(atuin history end ... &amp;)` shell integration.</param>
    public static void HistoryEnd(string id, long exitCode, long durationNs, bool sync = false)
    {
        IntPtr error = NativeMethods.HistoryEnd(id, exitCode, durationNs, sync ? 1 : 0);
        if (error != IntPtr.Zero)
        {
            throw new InvalidOperationException(
                $"atuin HistoryEnd failed: {TakeError(error)}");
        }
    }

    /// <summary>
    /// Run a non-interactive search with upstream-compatible options.
    /// Returns the matching commands.
    /// </summary>
    public static string[] Search(AtuinSearchOptions options)
    {
        ArgumentNullException.ThrowIfNull(options);

        var allocated = new List<IntPtr>();
        var arrays = new List<IntPtr>();
        try
        {
            var native = new AtuinSearchOptionsNative
            {
                SearchMode = (int)options.SearchMode,
                FilterMode = (int)options.FilterMode,
                HasLimit = options.Limit.HasValue ? 1 : 0,
                Limit = options.Limit ?? 0,
                HasOffset = options.Offset.HasValue ? 1 : 0,
                Offset = options.Offset ?? 0,
                Reverse = options.Reverse ? 1 : 0,
                IncludeDuplicates = options.IncludeDuplicates ? 1 : 0,
            };

            native.Query = ToNative(options.Query, allocated);
            native.Cwd = ToNative(options.Cwd, allocated);
            native.ExcludeCwd = ToNative(options.ExcludeCwd, allocated);
            native.Before = ToNative(options.Before, allocated);
            native.After = ToNative(options.After, allocated);

            native.Exits = ToI64Array(options.Exits, arrays);
            native.ExitCount = (UIntPtr)(options.Exits?.Length ?? 0);
            native.ExcludeExits = ToI64Array(options.ExcludeExits, arrays);
            native.ExcludeExitCount = (UIntPtr)(options.ExcludeExits?.Length ?? 0);

            native.Authors = ToArray(options.Authors, allocated, arrays);
            native.AuthorCount = (UIntPtr)(options.Authors?.Length ?? 0);
            native.Shells = ToArray(options.Shells, allocated, arrays);
            native.ShellCount = (UIntPtr)(options.Shells?.Length ?? 0);

            IntPtr error = NativeMethods.Search(in native, out IntPtr outPtr);
            if (error != IntPtr.Zero)
            {
                throw new InvalidOperationException(
                    $"atuin Search failed: {TakeError(error)}");
            }
            if (outPtr == IntPtr.Zero)
            {
                throw new InvalidOperationException(
                    "atuin Search succeeded but returned no result string");
            }

            try
            {
                return SplitResults(outPtr);
            }
            finally
            {
                NativeMethods.Free(outPtr);
            }
        }
        finally
        {
            foreach (IntPtr ptr in arrays) Marshal.FreeHGlobal(ptr);
            foreach (IntPtr ptr in allocated) Marshal.FreeCoTaskMem(ptr);
        }
    }

    private static string[] SplitResults(IntPtr ptr)
    {
        string text = Marshal.PtrToStringUTF8(ptr) ?? string.Empty;
        return text.Length == 0
            ? Array.Empty<string>()
            : text.Split('\n', StringSplitOptions.None);
    }

    private static IntPtr ToNative(string? value, List<IntPtr> allocated)
    {
        if (value is null)
        {
            return IntPtr.Zero;
        }
        IntPtr ptr = Marshal.StringToCoTaskMemUTF8(value);
        allocated.Add(ptr);
        return ptr;
    }

    private static IntPtr ToArray(string[]? values, List<IntPtr> allocated, List<IntPtr> arrays)
    {
        if (values is null || values.Length == 0)
        {
            return IntPtr.Zero;
        }

        IntPtr array = Marshal.AllocHGlobal(IntPtr.Size * values.Length);
        arrays.Add(array);
        for (int i = 0; i < values.Length; i++)
        {
            IntPtr item = ToNative(values[i], allocated);
            Marshal.WriteIntPtr(array, i * IntPtr.Size, item);
        }
        return array;
    }

    /// <summary>
    /// Marshal a 64-bit integer array to unmanaged memory. Returns
    /// <see cref="IntPtr.Zero"/> for null/empty arrays, which the native side
    /// treats as "no restriction".
    /// </summary>
    private static IntPtr ToI64Array(long[]? values, List<IntPtr> arrays)
    {
        if (values is null || values.Length == 0)
        {
            return IntPtr.Zero;
        }

        IntPtr array = Marshal.AllocHGlobal(sizeof(long) * values.Length);
        arrays.Add(array);
        Marshal.Copy(values, 0, array, values.Length);
        return array;
    }

    /// <summary>
    /// Prefix search. Returns the matching commands, newest first.
    /// </summary>
    /// <param name="query">Prefix query. Null matches the empty prefix.</param>
    /// <param name="limit">Maximum number of results.</param>
    public static string[] SearchPrefix(string? query, int limit = 10)
    {
        if (limit < 0) limit = 0;

        IntPtr error = NativeMethods.SearchPrefix(query, limit, out IntPtr outPtr);
        if (error != IntPtr.Zero)
        {
            throw new InvalidOperationException(
                $"atuin SearchPrefix failed: {TakeError(error)}");
        }
        if (outPtr == IntPtr.Zero)
        {
            throw new InvalidOperationException(
                "atuin SearchPrefix succeeded but returned no result string");
        }

        try
        {
            return SplitResults(outPtr);
        }
        finally
        {
            NativeMethods.Free(outPtr);
        }
    }

    /// <summary>
    /// Run the in-process full-screen interactive search TUI (the official
    /// <c>atuin search -i</c> replacement).
    /// </summary>
    /// <param name="query">Initial query. Null matches the empty query.</param>
    /// <param name="shellUpKeyBinding">Mirror <c>--shell-up-key-binding</c>
    /// (the shell's UpArrow widget).</param>
    /// <param name="keymapMode">Mirror <c>--keymap-mode</c> (used by the vi widgets).</param>
    /// <returns>The selected command, or null when the user cancelled.
    /// The returned command may be prefixed with
    /// <c>__atuin_accept__:</c> when it should be executed immediately.</returns>
    public static string? SearchInteractive(
        string? query,
        bool shellUpKeyBinding = false,
        AtuinKeymapMode keymapMode = AtuinKeymapMode.Auto)
    {
        IntPtr error = NativeMethods.SearchInteractive(
            query ?? string.Empty,
            shellUpKeyBinding ? 1 : 0,
            (int)keymapMode,
            out IntPtr outPtr);
        if (error != IntPtr.Zero)
        {
            throw new InvalidOperationException(
                $"atuin SearchInteractive failed: {TakeError(error)}");
        }
        if (outPtr == IntPtr.Zero)
        {
            // Successful call with no selection: the user cancelled.
            return null;
        }

        try
        {
            return Marshal.PtrToStringUTF8(outPtr);
        }
        finally
        {
            NativeMethods.Free(outPtr);
        }
    }

    /// <summary>Read the session's operation-counter snapshot.</summary>
    public static AtuinStats GetStats()
    {
        IntPtr error = NativeMethods.Stats(out AtuinStats stats);
        if (error != IntPtr.Zero)
        {
            throw new InvalidOperationException(
                $"atuin stats failed: {TakeError(error)}");
        }
        return stats;
    }

    /// <summary>Human-readable summary of the session counters.</summary>
    public static string GetStatsReport()
    {
        AtuinStats s = GetStats();
        return $"uptime: {s.UptimeSecs}s, " +
               $"history: {s.HistoryStarts} starts, {s.HistoryEndsSync} sync ends, " +
               $"{s.HistoryEndsAsync} async ends ({s.InFlightHistoryEnds} in flight), " +
               $"searches: {s.SearchCalls} total, {s.SearchPrefixCalls} prefix, " +
               $"{s.InteractiveSearchCalls} interactive " +
               $"({s.InteractiveSelections} selected, {s.InteractiveCancels} cancelled)";
    }

    /// <summary>Return the session UUID.</summary>
    public static string SessionUuid()
    {
        IntPtr error = NativeMethods.SessionUuid(out IntPtr ptr);
        if (error != IntPtr.Zero)
        {
            throw new InvalidOperationException(
                $"atuin SessionUuid failed: {TakeError(error)}");
        }
        if (ptr == IntPtr.Zero)
        {
            throw new InvalidOperationException(
                "SessionUuid succeeded but returned no UUID");
        }
        return Marshal.PtrToStringUTF8(ptr) ?? string.Empty;
    }

    /// <summary>
    /// Get the library version string.
    /// </summary>
    public static string Version()
    {
        IntPtr ptr = NativeMethods.Version();
        return Marshal.PtrToStringUTF8(ptr) ?? "unknown";
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    /// <summary>
    /// Consume an error pointer returned by a native call: NULL means success
    /// (returns null), otherwise the message is copied into a managed string
    /// and the native allocation is released with zo_free.
    /// </summary>
    private static string? TakeError(IntPtr error)
    {
        if (error == IntPtr.Zero)
            return null;
        string? message = Marshal.PtrToStringUTF8(error);
        NativeMethods.Free(error);
        return message;
    }
}

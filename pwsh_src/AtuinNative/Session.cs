using System.Runtime.InteropServices;

namespace AtuinNative;

/// <summary>
/// Safe managed wrapper around the atuin-ffi native session.
///
/// The native session persists for the lifetime of this object and keeps the
/// SQLite connections, settings, encryption key and tokio runtime alive
/// between calls — the optimization that avoids per-command process startup
/// and per-command database/key loading.
/// </summary>
public sealed class AtuinSession : IDisposable
{
    private IntPtr _handle;
    private bool _disposed;

    /// <summary>
    /// Create a new history session.
    /// </summary>
    /// <param name="dataDir">Atuin data directory, or null for the default.</param>
    public AtuinSession(string? dataDir = null)
    {
        _handle = NativeMethods.SessionCreate(dataDir);
        if (_handle == IntPtr.Zero)
        {
            throw new InvalidOperationException(
                $"Failed to create atuin session: {LastError() ?? "unknown error"}");
        }
    }

    /// <summary>
    /// Record a command start and return the history ID.
    /// </summary>
    public string HistoryStart(string command, string cwd = "")
    {
        ArgumentNullException.ThrowIfNull(command);
        ObjectDisposedException.ThrowIf(_disposed, this);

        int rc = NativeMethods.HistoryStart(
            _handle, command, cwd ?? string.Empty, null, null, out IntPtr idPtr);
        if (rc != 0 || idPtr == IntPtr.Zero)
        {
            throw new InvalidOperationException(
                $"HistoryStart failed (rc={rc}): {LastError() ?? "unknown error"}");
        }

        try
        {
            return Marshal.PtrToStringUTF8(idPtr) ?? string.Empty;
        }
        finally
        {
            NativeMethods.FreeString(idPtr);
        }
    }

    /// <summary>
    /// Finalize a command.
    /// </summary>
    /// <param name="sync">When true, block until the database update completes
    /// and throw on failure. When false, schedule fire-and-forget work like
    /// the official `(atuin history end ... &amp;)` shell integration.</param>
    public void HistoryEnd(string id, long exitCode, long durationNs, bool sync = false)
    {
        ArgumentNullException.ThrowIfNull(id);
        ObjectDisposedException.ThrowIf(_disposed, this);

        int rc = NativeMethods.HistoryEnd(
            _handle, id, exitCode, durationNs, sync ? 1 : 0);
        if (rc != 0 && sync)
        {
            throw new InvalidOperationException(
                $"HistoryEnd failed (rc={rc}): {LastError() ?? "unknown error"}");
        }
    }

    /// <summary>
    /// Prefix search. Returns the matching commands, newest first.
    /// </summary>
    /// <param name="query">Prefix query. Null matches the empty prefix.</param>
    /// <param name="limit">Maximum number of results.</param>
    public string[] SearchPrefix(string? query, int limit = 10)
    {
        ObjectDisposedException.ThrowIf(_disposed, this);
        if (limit < 0) limit = 0;

        int rc = NativeMethods.SearchPrefix(_handle, query, limit, out IntPtr outPtr);
        if (rc != 0 || outPtr == IntPtr.Zero)
        {
            throw new InvalidOperationException(
                $"SearchPrefix failed (rc={rc}): {LastError() ?? "unknown error"}");
        }

        try
        {
            string text = Marshal.PtrToStringUTF8(outPtr) ?? string.Empty;
            return text.Length == 0
                ? Array.Empty<string>()
                : text.Split('\n', StringSplitOptions.None);
        }
        finally
        {
            NativeMethods.FreeString(outPtr);
        }
    }

    /// <summary>Return the session UUID.</summary>
    public string SessionUuid()
    {
        ObjectDisposedException.ThrowIf(_disposed, this);
        IntPtr ptr = NativeMethods.SessionUuid(_handle);
        if (ptr == IntPtr.Zero)
        {
            throw new InvalidOperationException(
                $"SessionUuid failed: {LastError() ?? "unknown error"}");
        }
        return Marshal.PtrToStringUTF8(ptr) ?? string.Empty;
    }

    /// <summary>Return the native library version string.</summary>
    public static string VersionStr()
    {
        IntPtr ptr = NativeMethods.Version();
        return Marshal.PtrToStringUTF8(ptr) ?? string.Empty;
    }

    /// <summary>
    /// Return the last native error message, or null when no error is set.
    /// The native pointer is only valid until the next FFI call, so the value
    /// is copied before returning.
    /// </summary>
    public static string? LastError()
    {
        IntPtr ptr = NativeMethods.LastError();
        return ptr == IntPtr.Zero ? null : Marshal.PtrToStringUTF8(ptr);
    }

    public void Dispose()
    {
        if (!_disposed && _handle != IntPtr.Zero)
        {
            NativeMethods.SessionDestroy(_handle);
            _handle = IntPtr.Zero;
        }
        _disposed = true;
        GC.SuppressFinalize(this);
    }
}

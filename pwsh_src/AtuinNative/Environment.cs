using System.Runtime.InteropServices;

namespace AtuinNative;

/// <summary>
/// Cross-platform environment variable helper.
///
/// On Linux/macOS, .NET's <c>Environment.SetEnvironmentVariable</c> and
/// PowerShell's <c>$env:</c> only affect the .NET-managed environment block —
/// they do NOT call the native C library's <c>setenv()</c>. This means
/// in-process native code (like the Rust atuin-ffi library loaded via
/// P/Invoke) cannot see those variables via <c>getenv()</c> /
/// <c>std::env::var()</c>.
///
/// Use <see cref="Set"/> for variables that the embedded Rust library reads
/// (for example ATUIN_SHELL and ATUIN_SESSION).
/// </summary>
public static partial class AtuinEnvironment
{
    /// <summary>
    /// Set an environment variable visible to both .NET and native in-process
    /// code.
    /// </summary>
    public static void Set(string name, string value)
    {
        ArgumentNullException.ThrowIfNull(name);
        ArgumentNullException.ThrowIfNull(value);

        // 1. .NET environment (for subprocess compatibility, $env: reads, etc.)
        Environment.SetEnvironmentVariable(name, value);

        // 2. Native OS environment (for in-process getenv() / std::env::var())
        if (!RuntimeInformation.IsOSPlatform(OSPlatform.Windows))
        {
            _ = SetEnv(name, value, 1);
        }
    }

    /// <summary>
    /// Read an environment variable. On Unix the native block is authoritative
    /// for in-process native code; the managed block is used on Windows.
    /// </summary>
    public static string? Get(string name)
    {
        ArgumentNullException.ThrowIfNull(name);

        if (RuntimeInformation.IsOSPlatform(OSPlatform.Windows))
        {
            return Environment.GetEnvironmentVariable(name);
        }

        IntPtr ptr = GetEnv(name);
        return ptr == IntPtr.Zero ? null : Marshal.PtrToStringUTF8(ptr);
    }

    /// <summary>Remove an environment variable from both blocks.</summary>
    public static void Remove(string name)
    {
        ArgumentNullException.ThrowIfNull(name);

        Environment.SetEnvironmentVariable(name, null);

        if (!RuntimeInformation.IsOSPlatform(OSPlatform.Windows))
        {
            _ = UnsetEnv(name);
        }
    }

    private const string LibC = "libc";

    [LibraryImport(LibC, EntryPoint = "setenv", StringMarshalling = StringMarshalling.Utf8)]
    private static partial int SetEnv(string name, string value, int overwrite);

    [LibraryImport(LibC, EntryPoint = "unsetenv", StringMarshalling = StringMarshalling.Utf8)]
    private static partial int UnsetEnv(string name);

    [LibraryImport(LibC, EntryPoint = "getenv", StringMarshalling = StringMarshalling.Utf8)]
    private static partial IntPtr GetEnv(string name);
}

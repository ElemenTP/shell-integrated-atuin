using AtuinNative;

// Self-contained managed unit test runner for AtuinNative.dll.
//
// It deliberately has no external test-framework dependency so it can run on a
// minimal machine with just the .NET SDK. The native library is loaded through
// the ATUIN_FFI_PATH environment variable (set by CMake / the developer).

int passed = 0;
int failed = 0;

void Check(bool condition, string message)
{
    if (condition)
    {
        passed++;
        Console.WriteLine($"PASS  {message}");
    }
    else
    {
        failed++;
        Console.Error.WriteLine($"FAIL  {message}");
    }
}

void CheckEqual<T>(T actual, T expected, string message)
{
    Check(EqualityComparer<T>.Default.Equals(actual, expected),
        $"{message} (actual={actual}, expected={expected})");
}

void Throws<TException>(Action action, string message) where TException : Exception
{
    try
    {
        action();
        Check(false, $"{message} (no exception thrown)");
    }
    catch (TException)
    {
        Check(true, message);
    }
    catch (Exception e)
    {
        Check(false, $"{message} (wrong exception: {e.GetType().Name})");
    }
}

string ffiPath = Environment.GetEnvironmentVariable("ATUIN_FFI_PATH")
    ?? throw new InvalidOperationException("ATUIN_FFI_PATH must point at libatuin_ffi.so / .dylib / atuin_ffi.dll");
if (!File.Exists(ffiPath))
{
    throw new FileNotFoundException("Native atuin-ffi library not found", ffiPath);
}

string dataDir = Path.Combine(Path.GetTempPath(), $"atuin-cs-unit-{Environment.ProcessId}-{Guid.NewGuid():N}");
Directory.CreateDirectory(dataDir);

try
{
    // ── Static metadata ────────────────────────────────────────────────
    CheckEqual(AtuinSession.VersionStr(), "0.1.0", "native version");
    Check(AtuinSession.LastError() is null, "last error is initially null");

    // ── Environment helper ─────────────────────────────────────────────
    AtuinEnvironment.Set("ATUIN_NATIVE_TEST_VAR", "hello-native");
    CheckEqual(AtuinEnvironment.Get("ATUIN_NATIVE_TEST_VAR"), "hello-native", "environment Set/Get round-trip");
    AtuinEnvironment.Remove("ATUIN_NATIVE_TEST_VAR");
    Check(AtuinEnvironment.Get("ATUIN_NATIVE_TEST_VAR") is null, "environment Remove");

    // ── Session lifecycle ──────────────────────────────────────────────
    using (var session = new AtuinSession(dataDir))
    {
        string uuid1 = session.SessionUuid();
        string uuid2 = session.SessionUuid();
        Check(!string.IsNullOrEmpty(uuid1), "session UUID non-empty");
        CheckEqual(uuid1, uuid2, "session UUID stable");

        // ── History round-trip ──────────────────────────────────────────
        string id = session.HistoryStart("echo csharp-unit-test", "/tmp");
        Check(!string.IsNullOrEmpty(id), "HistoryStart returns an ID");
        session.HistoryEnd(id, 0, 987654, sync: true);
        Check(true, "HistoryEnd sync completes");

        string[] results = session.SearchPrefix("echo csharp-unit-test", 5);
        Check(results.Length == 1 && results[0] == "echo csharp-unit-test",
            "SearchPrefix finds recorded command");

        string[] limited = session.SearchPrefix("echo csharp-unit-test", 1);
        CheckEqual(limited.Length, 1, "SearchPrefix limit=1");

        string[] none = session.SearchPrefix("no-such-command-xyz", 5);
        CheckEqual(none.Length, 0, "SearchPrefix missing query returns empty");

        string[] zero = session.SearchPrefix("echo csharp-unit-test", 0);
        CheckEqual(zero.Length, 0, "SearchPrefix limit=0 returns empty");

        // ── Argument validation ─────────────────────────────────────────
        Throws<ArgumentNullException>(() => session.HistoryStart(null!), "HistoryStart rejects null command");
        Throws<ArgumentNullException>(() => session.HistoryEnd(null!, 0, 0, true), "HistoryEnd rejects null id");
    }

    // ── Disposed behavior ──────────────────────────────────────────────
    var disposed = new AtuinSession(dataDir);
    disposed.Dispose();
    disposed.Dispose(); // must be idempotent
    Check(true, "double Dispose is safe");
    Throws<ObjectDisposedException>(() => disposed.SearchPrefix("echo", 1), "SearchPrefix after Dispose throws");

    // ── Multiple independent sessions ───────────────────────────────────
    for (int i = 0; i < 3; i++)
    {
        using var s = new AtuinSession(dataDir);
        string id = s.HistoryStart($"echo multi-session-{i}", "/tmp");
        s.HistoryEnd(id, 0, 0, sync: true);
    }
    Check(true, "multiple independent sessions");
}
finally
{
    try { Directory.Delete(dataDir, recursive: true); } catch { /* best effort */ }
}

Console.WriteLine($"\nResults: {passed} passed, {failed} failed");
return failed > 0 ? 1 : 0;

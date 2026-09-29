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

// HistoryStart returns null when Atuin's filters drop a command; the tests
// below record ordinary commands and therefore require an ID.
string RequireId(string? id)
{
    if (string.IsNullOrEmpty(id))
    {
        throw new InvalidOperationException("expected a history ID");
    }
    return id;
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
    // ── Isolated settings tree ─────────────────────────────────────────
    // atuin_init takes no data directory: sessions follow the same
    // settings resolution as the official CLI. Point the settings tree at the
    // temp directory so the tests never touch the real ~/.config/atuin.
    string configDir = Path.Combine(dataDir, "config");
    Directory.CreateDirectory(configDir);
    File.WriteAllText(
        Path.Combine(configDir, "config.toml"),
        $"data_dir = \"{dataDir.Replace('\\', '/')}\"\n");
    AtuinEnvironment.Set("ATUIN_CONFIG_DIR", configDir);
    AtuinEnvironment.Set("ATUIN_DATA_DIR", dataDir);

    // ── Static metadata ────────────────────────────────────────────────
    CheckEqual(Session.Version(), "0.1.0", "native version");

    // ── Session lifecycle ──────────────────────────────────────────────
    Session.Initialize();
    try
    {
        string uuid1 = Session.SessionUuid();
        string uuid2 = Session.SessionUuid();
        Check(!string.IsNullOrEmpty(uuid1), "session UUID non-empty");
        CheckEqual(uuid1, uuid2, "session UUID stable");

        // The library owns a single process-wide session; a second Initialize is
        // idempotent and keeps that session.
        Session.Initialize();
        CheckEqual(Session.SessionUuid(), uuid2,
            "second Initialize keeps the active session");

        // ── History round-trip ──────────────────────────────────────────
        string id = RequireId(Session.HistoryStart("echo csharp-unit-test", "/tmp"));
        Check(!string.IsNullOrEmpty(id), "HistoryStart returns an ID");
        Session.HistoryEnd(id, 0, 987654, sync: true);
        Check(true, "HistoryEnd sync completes");

        string[] results = Session.SearchPrefix("echo csharp-unit-test", 5);
        Check(results.Length == 1 && results[0] == "echo csharp-unit-test",
            "SearchPrefix finds recorded command");

        var genericOptions = new AtuinSearchOptions {
            Query = "echo csharp-unit-test",
            SearchMode = AtuinSearchMode.Prefix,
            Limit = 5,
            Authors = new[] { "$all-user" },
        };
        string[] generic = Session.Search(genericOptions);
        Check(generic.Length == 1 && generic[0] == "echo csharp-unit-test",
            "Search with upstream-compatible options finds recorded command");

        // ── author_kind / repeatable exit filters ───────────────────────
        string agentId = RequireId(Session.HistoryStart(
            "echo csharp-agent-kind", "/tmp", "claude", AtuinAuthorKind.Agent, "why"));
        Session.HistoryEnd(agentId, 0, 0, sync: true);
        var agentFilter = new AtuinSearchOptions {
            Query = "echo csharp-agent-kind",
            SearchMode = AtuinSearchMode.Prefix,
            Authors = new[] { "$all-agent" },
        };
        Check(Session.Search(agentFilter).Length == 1,
            "stated agent author_kind is recorded");

        string exitId = RequireId(Session.HistoryStart("echo csharp-exit-filter", "/tmp"));
        Session.HistoryEnd(exitId, 7, 0, sync: true);
        var includeExit = new AtuinSearchOptions {
            Query = "echo csharp-exit-filter",
            SearchMode = AtuinSearchMode.Prefix,
            Exits = new long[] { 7, 130 },
        };
        Check(Session.Search(includeExit).Length == 1,
            "repeatable --exit includes a matching code");
        var excludeExit = new AtuinSearchOptions {
            Query = "echo csharp-exit-filter",
            SearchMode = AtuinSearchMode.Prefix,
            ExcludeExits = new long[] { 7 },
        };
        Check(Session.Search(excludeExit).Length == 0,
            "repeatable --exclude-exit drops a matching code");

        string[] limited = Session.SearchPrefix("echo csharp-unit-test", 1);
        CheckEqual(limited.Length, 1, "SearchPrefix limit=1");

        string[] none = Session.SearchPrefix("no-such-command-xyz", 5);
        CheckEqual(none.Length, 0, "SearchPrefix missing query returns empty");

        string[] zero = Session.SearchPrefix("echo csharp-unit-test", 0);
        CheckEqual(zero.Length, 0, "SearchPrefix limit=0 returns empty");

        // ── Call-local errors: a failed call returns its own error and the
        // session keeps working afterwards.
        Throws<InvalidOperationException>(
            () => Session.HistoryEnd("not-a-uuid", 0, 0, true),
            "HistoryEnd returns an error string for an invalid ID");
        string afterError = RequireId(Session.HistoryStart("echo after-call-local-error", "/tmp"));
        Session.HistoryEnd(afterError, 0, 0, sync: true);
        Check(true, "session still works after a failed call");

        // ── Input validation ────────────────────────────────────────────
        Throws<ArgumentOutOfRangeException>(
            () => Session.HistoryEnd(id, 0, -1, sync: true),
            "HistoryEnd rejects a negative duration");
        Throws<ArgumentOutOfRangeException>(
            () => Session.Search(new AtuinSearchOptions { Query = "echo", Limit = -1 }),
            "Search rejects a negative limit");
        Throws<ArgumentOutOfRangeException>(
            () => Session.Search(new AtuinSearchOptions { Query = "echo", Offset = -5 }),
            "Search rejects a negative offset");

        // A command dropped by Atuin's filters is a successful call with no ID,
        // not an error. A leading space is filtered out by default.
        string? filtered = Session.HistoryStart(" filtered-out", "/tmp");
        Check(filtered is null, "HistoryStart returns null for a filtered command");

        // ── Session stats ───────────────────────────────────────────────
        AtuinStats stats = Session.GetStats();
        Check(stats.HistoryStarts >= 1, "stats counts history starts");
        Check(stats.HistoryEndsSync >= 1, "stats counts sync history ends");
        Check(stats.SearchPrefixCalls >= 1, "stats counts prefix searches");
        Check(stats.SearchCalls >= stats.SearchPrefixCalls, "stats search total covers prefix");
        Check(!string.IsNullOrEmpty(Session.GetStatsReport()), "stats report is non-empty");
    }
    finally
    {
        Session.Shutdown();
    }

    // ── Shutdown is idempotent; calls without a session are rejected ────
    Session.Shutdown();
    Session.Shutdown(); // must be idempotent
    Check(true, "double Shutdown is safe");
    Throws<InvalidOperationException>(
        () => Session.SearchPrefix("echo", 1), "SearchPrefix without a session throws");
    Throws<InvalidOperationException>(
        () => Session.SearchInteractive("echo"), "SearchInteractive without a session throws");

    // ── Repeated create/shutdown cycles ─────────────────────────────────
    for (int i = 0; i < 3; i++)
    {
        Session.Initialize();
        AtuinStats fresh = Session.GetStats();
        Check(fresh.HistoryStarts == 0 && fresh.SearchCalls == 0,
            $"re-created session {i} starts with zero stats");
        string id = RequireId(Session.HistoryStart($"echo multi-session-{i}", "/tmp"));
        Session.HistoryEnd(id, 0, 0, sync: true);
        Session.Shutdown();
    }
    Check(true, "repeated create/shutdown cycles");
}
finally
{
    try { Directory.Delete(dataDir, recursive: true); } catch { /* best effort */ }
}

Console.WriteLine($"\nResults: {passed} passed, {failed} failed");
return failed > 0 ? 1 : 0;

// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json;
using System.Text.Json.Serialization;

namespace Infinity.Core.Process;

/// <summary>Who holds <c>launcher.lock</c>.</summary>
public sealed record LockHolder(
    [property: JsonPropertyName("pid")] uint Pid,
    [property: JsonPropertyName("started_unix")] ulong StartedUnix,
    [property: JsonPropertyName("sid")] string? Sid);

/// <summary>
/// One launcher at a time (ORCHESTRATOR §1.8; lock.rs): <c>&lt;state&gt;\launcher.lock</c> opened for
/// writing with share mode read only, so a second writer gets a sharing violation while a reader
/// can still see who holds it. The OS drops the lock when the holder dies (the game never
/// inherits handles), so a file left by a dead launcher opens normally and is rewritten.
/// </summary>
public sealed class LauncherLock : IDisposable
{
    public const string FileName = "launcher.lock";

    /// <summary>The main window's title, which a second instance brings to the front.</summary>
    public const string WindowTitle = "Infinity Launcher";

    private const int SharingViolation = unchecked((int)0x80070020);

    private readonly FileStream _file;

    private LauncherLock(FileStream file, string path, LockHolder holder)
    {
        _file = file;
        Path = path;
        Holder = holder;
    }

    public string Path { get; }

    public LockHolder Holder { get; private set; }

    /// <summary>Takes the lock for this process, or returns false with the holder (null when its record cannot be read yet).</summary>
    public static bool TryAcquire(string stateDirectory, ulong startedUnix, out LauncherLock? acquired, out LockHolder? heldBy)
    {
        Directory.CreateDirectory(stateDirectory);
        var path = System.IO.Path.Combine(stateDirectory, FileName);
        FileStream file;
        try
        {
            file = new FileStream(path, FileMode.OpenOrCreate, FileAccess.ReadWrite, FileShare.Read);
        }
        catch (IOException e) when (e.HResult == SharingViolation)
        {
            acquired = null;
            heldBy = ReadHolder(stateDirectory);
            return false;
        }
        var holder = new LockHolder((uint)Environment.ProcessId, startedUnix, null);
        var lockFile = new LauncherLock(file, path, holder);
        lockFile.Write();
        acquired = lockFile;
        heldBy = null;
        return true;
    }

    /// <summary>Reads who holds the lock, without disturbing it.</summary>
    public static LockHolder? ReadHolder(string stateDirectory)
    {
        try
        {
            using var file = new FileStream(System.IO.Path.Combine(stateDirectory, FileName), FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete);
            return JsonSerializer.Deserialize<LockHolder>(file);
        }
        catch (Exception e) when (e is IOException or JsonException or UnauthorizedAccessException)
        {
            return null;
        }
    }

    /// <summary>Records the session being supervised (or none).</summary>
    public void SetSid(string? sid)
    {
        Holder = Holder with { Sid = sid };
        Write();
    }

    private void Write()
    {
        var bytes = JsonSerializer.SerializeToUtf8Bytes(Holder);
        _file.SetLength(0);
        _file.Position = 0;
        _file.Write(bytes);
        _file.Flush(flushToDisk: true);
    }

    public void Dispose() => _file.Dispose();
}

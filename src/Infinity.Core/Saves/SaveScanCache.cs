// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Diagnostics;
using System.Globalization;
using System.Security.Cryptography;
using System.Text;

namespace Infinity.Core.Saves;

/// <summary>
/// Owner 2026-11-10 ("our launcher must be fluid, nothing should get freezed"): a sealed-save scan hashes every file of every
/// revision (<see cref="AzimuthSaves.Verify"/>), and the shell's cards read it many times per refresh. This cache keeps each answer
/// with a stamp of its folder (every file's relative path, length and write time, from one directory listing, no file read): the same
/// stamp answers from memory, any change on disk recomputes. <see cref="Prime"/> runs the expensive first computation on a worker so the
/// UI thread then only lists folders. Answers are the same objects <see cref="AzimuthSaves"/> returns (immutable records), safe to share
/// across threads.
/// </summary>
public sealed class SaveScanCache(TimeProvider? clock = null)
{
    private readonly TimeProvider _clock = clock ?? TimeProvider.System;
    private readonly Lock _gate = new();
    private readonly Dictionary<string, Entry> _entries = new(StringComparer.OrdinalIgnoreCase);
    private long _epoch;
    private long _computations;

    private sealed record Entry(string Stamp, object? Value, long CheckedAt, long Epoch, string Folder, Func<object?> Compute);

    /// <summary>
    /// Owner 2026-11-10 (the window never freezes): an answer already checked in the current epoch is used without listing its folder
    /// again; only <see cref="Invalidate"/> or <see cref="Prime"/> (which re-checks every known answer on its worker) makes it look again.
    /// The shell sets it: its UI thread then never lists a saves tree after the off-thread scan. False keeps <see cref="Fresh"/>'s rule.
    /// </summary>
    public bool TrustWithinEpoch { get; init; }

    /// <summary>
    /// For this long after a stamp was checked the answer is used without listing the folder again. Zero by default: every read lists
    /// the folder (cheap, nothing opened), so a write followed at once by a read (a clone, then the cards) is always seen.
    /// <see cref="Invalidate"/> ends every window at once.
    /// </summary>
    public TimeSpan Fresh { get; init; } = TimeSpan.Zero;

    /// <summary>How many answers were computed (not served from memory): tests and the dispatcher watchdog's log.</summary>
    public long Computations => Interlocked.Read(ref _computations);

    /// <summary>The next read checks every stamp again (the answers stay and are reused when their folder did not change).</summary>
    public void Invalidate() => Interlocked.Increment(ref _epoch);

    /// <summary>Forgets everything (sign-out, another Azimuth folder).</summary>
    public void Clear()
    {
        lock (_gate) _entries.Clear();
        Invalidate();
    }

    /// <summary><see cref="AzimuthSaves.List"/>, cached on the stamp of <c>&lt;savesRoot&gt;\player-1</c>.</summary>
    public IReadOnlyList<SaveProfile> List(string savesRoot, string? exclude = null)
    {
        ArgumentNullException.ThrowIfNull(savesRoot);
        return (IReadOnlyList<SaveProfile>)Memo("list|" + savesRoot + "|" + exclude, Path.Combine(savesRoot, "player-1"),
            () => AzimuthSaves.List(savesRoot, exclude))!;
    }

    /// <summary><see cref="AzimuthSaves.NewestSealed"/> of one <c>User_*</c> folder, cached on its stamp.</summary>
    public LocalRevision? NewestSealed(string profileDir)
    {
        ArgumentNullException.ThrowIfNull(profileDir);
        return (LocalRevision?)Memo("newest|" + profileDir, profileDir, () => AzimuthSaves.NewestSealed(profileDir));
    }

    /// <summary>A digest of one revision's content (<paramref name="digest"/>, e.g. a bundle's content digest), cached on its folder's stamp.</summary>
    public string Digest(LocalRevision revision, string kind, Func<LocalRevision, string> digest)
    {
        ArgumentNullException.ThrowIfNull(revision);
        ArgumentNullException.ThrowIfNull(digest);
        return (string)Memo("digest|" + kind + "|" + revision.Path, revision.Path, () => digest(revision))!;
    }

    /// <summary>
    /// Off the UI thread: every <c>User_*</c> folder of each saves root scanned once (the list, each newest revision and, when
    /// <paramref name="digest"/> is given for a root, its content digest), so the UI thread later only checks stamps. True when an answer
    /// had to be computed (something changed since the last scan). Errors are left to the later read, which reports them as before.
    /// </summary>
    public bool Prime(IEnumerable<(string SavesRoot, string? DigestKind, Func<LocalRevision, string>? Digest)> roots)
    {
        ArgumentNullException.ThrowIfNull(roots);
        var before = Computations;
        // every answer known so far is checked again (its folder listed here, on the caller's worker), so a trusted one never goes stale
        KeyValuePair<string, Entry>[] known;
        lock (_gate) known = [.. _entries];
        foreach (var (key, entry) in known)
        {
            // a revision that is gone (pruned, moved away) keeps no digest
            if (key.StartsWith("digest|", StringComparison.Ordinal) && !Directory.Exists(entry.Folder)) { lock (_gate) _entries.Remove(key); continue; }
            try { Memo(key, entry.Folder, entry.Compute, force: true); }
            catch (Exception e) when (e is SaveException or IOException or UnauthorizedAccessException or DecoderFallbackException) { }
        }
        foreach (var (root, kind, digest) in roots)
        {
            try
            {
                List(root);
                var player = Path.Combine(root, "player-1");
                if (!Directory.Exists(player)) continue;
                foreach (var dir in Directory.EnumerateDirectories(player, "User_*"))
                {
                    if (NewestSealed(dir) is { } newest && digest is not null && kind is not null)
                    {
                        try { Digest(newest, kind, digest); }
                        catch (Exception e) when (e is SaveException or IOException or UnauthorizedAccessException or DecoderFallbackException) { }
                    }
                }
            }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException) { }
        }
        return Computations != before;
    }

    private object? Memo(string key, string folder, Func<object?> compute, bool force = false)
    {
        var now = _clock.GetTimestamp();
        var epoch = Interlocked.Read(ref _epoch);
        Entry? known;
        lock (_gate) _entries.TryGetValue(key, out known);
        if (!force && known is not null && known.Epoch == epoch && (TrustWithinEpoch || _clock.GetElapsedTime(known.CheckedAt, now) < Fresh)) return known.Value;
        var stamp = Stamp(folder);
        if (known is not null && known.Stamp == stamp)
        {
            lock (_gate) _entries[key] = known with { CheckedAt = now, Epoch = epoch };
            return known.Value;
        }
        var value = compute(); // an exception is not cached: the next read tries again
        Interlocked.Increment(ref _computations);
        lock (_gate) _entries[key] = new Entry(stamp, value, now, epoch, folder, compute);
        return value;
    }

    /// <summary>
    /// What changes when anything under <paramref name="folder"/> changes: every entry's relative path, length and write time (one
    /// listing; nothing is opened). "-" for a folder that does not exist.
    /// </summary>
    public static string Stamp(string folder)
    {
        ArgumentNullException.ThrowIfNull(folder);
        var root = new DirectoryInfo(folder);
        if (!root.Exists) return "-";
        var lines = new List<string>();
        try
        {
            foreach (var entry in root.EnumerateFileSystemInfos("*", new EnumerationOptions { RecurseSubdirectories = true, IgnoreInaccessible = true, AttributesToSkip = 0 }))
            {
                // a folder counts by its name only: NTFS updates a folder's time in its parent's index lazily, which would read as a change
                var (length, time) = entry is FileInfo file ? (file.Length, file.LastWriteTimeUtc.Ticks) : (-1L, 0L);
                lines.Add(Path.GetRelativePath(folder, entry.FullName) + "|" + length.ToString(CultureInfo.InvariantCulture) + "|"
                    + time.ToString(CultureInfo.InvariantCulture));
            }
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
            return "?" + Guid.NewGuid().ToString("N"); // unreadable now: never matches, so the answer is computed (and its error reported)
        }
        lines.Sort(StringComparer.Ordinal);
        return Convert.ToHexStringLower(SHA256.HashData(Encoding.UTF8.GetBytes(string.Join('\n', lines))));
    }

    /// <summary>Measures <paramref name="work"/> (ms), for the log lines that say how long a scan took.</summary>
    public static long Time(Action work)
    {
        ArgumentNullException.ThrowIfNull(work);
        var watch = Stopwatch.StartNew();
        work();
        return watch.ElapsedMilliseconds;
    }
}

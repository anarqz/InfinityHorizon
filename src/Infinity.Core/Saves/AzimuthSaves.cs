// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Security.Cryptography;
using System.Text;

namespace Infinity.Core.Saves;

/// <summary>A save folder that does not have the shape Azimuth writes, or a clone that did not verify.</summary>
/// <see cref="Landed"/>: the clone or install is already in the target's <c>revisions</c> (only its final
/// check failed); <see cref="LandedPath"/> is that folder when known.
public sealed class SaveException(string message, bool landed = false, string? landedPath = null) : Exception(message)
{
    public bool Landed { get; } = landed || landedPath is not null;

    public string? LandedPath { get; } = landedPath;
}

/// <summary>One line of an Azimuth commit file: <c>"&lt;sha256&gt; &lt;size&gt; &lt;name&gt;"</c>.</summary>
public sealed record CommitEntry(string Sha256, long Size, string Name);

/// <summary>One <c>revisions\&lt;16 hex&gt;</c> folder with a commit file that parses.</summary>
public sealed record LocalRevision(ulong Number, string Path, IReadOnlyList<CommitEntry> Entries);

/// <summary>
/// What the picker shows for one Azimuth profile folder (<c>User_&lt;hex&gt;</c>): its newest sealed
/// revision and the numbers ProfileData holds (null when a number is not found exactly once);
/// <see cref="Level"/> is the level the game shows (stored + 1).
/// </summary>
public sealed record SaveProfile(string Folder, ulong? Xuid, LocalRevision Revision, DateTime LastPlayedUtc, int? Level, int? Credits)
{
    /// <summary>Azimuth's own offline profile (<c>User_9000000000000</c>), the one a player without an account has.</summary>
    public bool IsOffline => Xuid == AzimuthSaves.OfflineXuid;
}

/// <summary>The result of <see cref="AzimuthSaves.Clone"/>.</summary>
public sealed record CloneResult(string Target, ulong Number, string RevisionPath, IReadOnlyList<string> Backups);

/// <summary>
/// Azimuth's save folders on this PC, the C# side of <c>crates/fi-launcher-core/src/saves.rs</c>
/// (same rules) plus OD-41: a new Infinity profile starts from a save the player picks.
/// <para>
/// Layout: <c>&lt;saves&gt;\player-1\User_&lt;XUID upper hex&gt;\revisions\&lt;16 lowercase hex&gt;\{commit, data\&lt;file&gt;}</c>,
/// where <c>&lt;saves&gt;</c> is <c>data\titles\1A96545B\saves</c> of the Azimuth folder. The commit file is
/// <c>Azimuth complete save revision v2</c>, <c>blobs N</c>, then N lines <c>sha256 size name</c>.
/// Azimuth loads the highest folder with a commit and keeps the two newest. A revision counts only
/// when it is sealed (every file matches its commit line).
/// </para>
/// Nothing here deletes: every revision a clone shadows, and the picked one, are first copied to
/// <c>&lt;saves&gt;\fi-launcher-archive\&lt;User_x&gt;\</c> (the Rust launcher's archive).
/// </summary>
public static class AzimuthSaves
{
    public const string CommitHeader = "Azimuth complete save revision v2";
    public const string ArchiveDir = "fi-launcher-archive";
    public const string IncomingDir = "fi-launcher-incoming";
    public const ulong OfflineXuid = 0x9000000000000;
    private const int MaxCommitBytes = 64 * 1024;

    /// <summary><c>&lt;azimuth&gt;\data\titles\1A96545B\saves</c>.</summary>
    public static string SavesRoot(string azimuthRoot)
    {
        ArgumentNullException.ThrowIfNull(azimuthRoot);
        return Path.Combine(azimuthRoot, "data", "titles", "1A96545B", "saves");
    }

    /// <summary><c>User_&lt;XUID upper hex&gt;</c> (ACCOUNTS.md §2).</summary>
    public static string FolderFor(ulong xuid) => "User_" + xuid.ToString("X", CultureInfo.InvariantCulture);

    /// <summary>Parses a commit file strictly (saves.rs <c>parse_commit</c>); null when it does not.</summary>
    public static IReadOnlyList<CommitEntry>? ParseCommit(string text)
    {
        ArgumentNullException.ThrowIfNull(text);
        var body = text.EndsWith('\n') ? text[..^1] : text;
        var lines = body.Split('\n').Select(l => l.EndsWith('\r') ? l[..^1] : l).ToList();
        if (lines.Count < 2 || lines[0] != CommitHeader || !lines[1].StartsWith("blobs ", StringComparison.Ordinal)
            || !int.TryParse(lines[1].AsSpan(6), NumberStyles.None, CultureInfo.InvariantCulture, out var blobs))
        {
            return null;
        }
        var entries = new List<CommitEntry>();
        foreach (var line in lines.Skip(2))
        {
            var parts = line.Split(' ', 3);
            if (parts.Length != 3
                || parts[0].Length != 64 || !parts[0].All(Uri.IsHexDigit)
                || !long.TryParse(parts[1], NumberStyles.None, CultureInfo.InvariantCulture, out var size)
                || !SafeName(parts[2])
                || entries.Any(e => string.Equals(e.Name, parts[2], StringComparison.OrdinalIgnoreCase)))
            {
                return null;
            }
            entries.Add(new CommitEntry(parts[0].ToLowerInvariant(), size, parts[2]));
        }
        return entries.Count == 0 || entries.Count != blobs ? null : entries;
    }

    /// <summary>
    /// A plain file name Windows keeps as written: ASCII letters, digits, '_', '-' and inner dots
    /// (Azimuth's are <c>ProfileData</c>, <c>SaveState</c>, …). No separators, drive prefixes,
    /// control characters, trailing dots or spaces, or device names.
    /// </summary>
    private static bool SafeName(string name) =>
        name.Length is > 0 and <= 64
        && name.All(c => char.IsAsciiLetterOrDigit(c) || c is '_' or '-' or '.')
        && name[0] != '.' && name[^1] != '.'
        && !ReservedNames.Contains(name.Split('.')[0]);

    private static readonly HashSet<string> ReservedNames = new(StringComparer.OrdinalIgnoreCase)
    {
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9",
        "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    };

    /// <summary>Every folder named exactly 16 hex digits, oldest first, with or without a commit.</summary>
    public static IReadOnlyList<(ulong Number, string Path)> RevisionFolders(string profileDir)
    {
        ArgumentNullException.ThrowIfNull(profileDir);
        var dir = Path.Combine(profileDir, "revisions");
        if (!Directory.Exists(dir))
        {
            return [];
        }
        return [.. Directory.EnumerateDirectories(dir)
            .Select(path => (Name: Path.GetFileName(path), Path: path))
            .Where(f => f.Name.Length == 16 && f.Name.All(Uri.IsHexDigit))
            .Select(f => (Number: ulong.Parse(f.Name, NumberStyles.AllowHexSpecifier, CultureInfo.InvariantCulture), f.Path))
            .OrderBy(f => f.Number)];
    }

    /// <summary>The highest 16-hex folder + 1 (every folder counts, torn ones too), or 1.</summary>
    public static ulong NextRevisionNumber(string profileDir) => RevisionFolders(profileDir) switch
    {
        { Count: 0 } => 1,
        var folders when folders[^1].Number == ulong.MaxValue => throw new SaveException("the profile's revision counter is exhausted"),
        var folders => folders[^1].Number + 1,
    };

    public static LocalRevision? ReadRevision(ulong number, string path)
    {
        var commit = Path.Combine(path, "commit");
        try
        {
            var info = new FileInfo(commit);
            if (!info.Exists || info.Length > MaxCommitBytes)
            {
                return null;
            }
            var entries = ParseCommit(new UTF8Encoding(false, true).GetString(File.ReadAllBytes(commit)));
            return entries is null ? null : new LocalRevision(number, path, entries);
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or DecoderFallbackException)
        {
            return null;
        }
    }

    /// <summary>Problems that make a revision unsealed (missing file, wrong size, wrong hash); empty when sealed.</summary>
    public static IReadOnlyList<string> Verify(LocalRevision revision)
    {
        ArgumentNullException.ThrowIfNull(revision);
        var problems = new List<string>();
        foreach (var entry in revision.Entries)
        {
            var path = Path.Combine(revision.Path, "data", entry.Name);
            try
            {
                var info = new FileInfo(path);
                if (!info.Exists)
                {
                    problems.Add(entry.Name + " is missing");
                }
                else if (info.Length != entry.Size)
                {
                    problems.Add($"{entry.Name} is {info.Length} bytes, commit says {entry.Size}");
                }
                else
                {
                    using var stream = File.OpenRead(path);
                    if (Convert.ToHexStringLower(SHA256.HashData(stream)) != entry.Sha256)
                    {
                        problems.Add(entry.Name + " does not match its commit hash");
                    }
                }
            }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException)
            {
                // Locked (Azimuth writing it) or unreadable: not sealed, as saves.rs records it. A lock is
                // not a torn revision: callers that move folders ask IsBusyFolder first.
                problems.Add(entry.Name + (IsBusy(e) ? " is busy: " : ": ") + e.Message);
            }
        }
        return problems;
    }

    /// <summary>A sharing or lock violation (another process, Azimuth, has the file open for writing).</summary>
    public static bool IsBusy(Exception e) =>
        e is IOException io && (io.HResult & 0xFFFF) is 32 /* ERROR_SHARING_VIOLATION */ or 33 /* ERROR_LOCK_VIOLATION */;

    /// <summary>
    /// True when a file under <paramref name="path"/> cannot be opened for reading with writers denied
    /// (a sharing or lock violation): someone is writing the folder, so it is neither torn nor sealed yet
    /// and must not be moved.
    /// </summary>
    public static bool IsBusyFolder(string path)
    {
        ArgumentNullException.ThrowIfNull(path);
        if (!Directory.Exists(path))
        {
            return false;
        }
        foreach (var file in Directory.EnumerateFiles(path, "*", SearchOption.AllDirectories))
        {
            try
            {
                using var stream = new FileStream(file, FileMode.Open, FileAccess.Read, FileShare.Read);
            }
            catch (IOException e) when (IsBusy(e))
            {
                return true;
            }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException)
            {
                // Gone or unreadable: not a writer's lock.
            }
        }
        return false;
    }

    /// <summary>The newest sealed revision; a torn newest one (still being written, or a crash) is skipped.</summary>
    public static LocalRevision? NewestSealed(string profileDir)
    {
        foreach (var (number, path) in RevisionFolders(profileDir).Reverse())
        {
            if (ReadRevision(number, path) is { } revision && Verify(revision).Count == 0)
            {
                return revision;
            }
        }
        return null;
    }

    /// <summary>The oldest sealed revision (CR-S51: a new career's first save, while Azimuth has not pruned it yet).</summary>
    public static LocalRevision? OldestSealed(string profileDir)
    {
        foreach (var (number, path) in RevisionFolders(profileDir))
        {
            if (ReadRevision(number, path) is { } revision && Verify(revision).Count == 0)
            {
                return revision;
            }
        }
        return null;
    }

    /// <summary>
    /// Every <c>User_*</c> folder of <c>player-1</c> with a sealed revision, most recently played
    /// first. <paramref name="exclude"/> leaves out one folder (the profile's own container).
    /// </summary>
    public static IReadOnlyList<SaveProfile> List(string savesRoot, string? exclude = null)
    {
        ArgumentNullException.ThrowIfNull(savesRoot);
        var player = Path.Combine(savesRoot, "player-1");
        if (!Directory.Exists(player))
        {
            return [];
        }
        var found = new List<SaveProfile>();
        foreach (var dir in Directory.EnumerateDirectories(player, "User_*"))
        {
            var folder = Path.GetFileName(dir);
            if (string.Equals(folder, exclude, StringComparison.OrdinalIgnoreCase) || NewestSealed(dir) is not { } revision)
            {
                continue;
            }
            ulong? xuid = ulong.TryParse(folder.AsSpan(5), NumberStyles.AllowHexSpecifier, CultureInfo.InvariantCulture, out var x) ? x : null;
            var played = File.GetLastWriteTimeUtc(Path.Combine(revision.Path, "commit"));
            var (level, credits) = Numbers(revision);
            found.Add(new SaveProfile(folder, xuid, revision, played, level, credits));
        }
        return [.. found.OrderByDescending(p => p.LastPlayedUtc)];
    }

    /// <summary>
    /// The level the game shows and the credits of a revision's ProfileData (each null when not found exactly once, or the file does not
    /// read): what the picker and the OD-133 career restore list show.
    /// </summary>
    public static (int? Level, int? Credits) Numbers(LocalRevision revision)
    {
        ArgumentNullException.ThrowIfNull(revision);
        if (!revision.Entries.Any(e => e.Name == "ProfileData"))
        {
            return (null, null);
        }
        byte[] profile;
        try
        {
            profile = File.ReadAllBytes(Path.Combine(revision.Path, "data", "ProfileData"));
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
            return (null, null); // listed without its numbers
        }
        // ProfileData's Level counts from 0: the game shows 7 for a stored 6 (seen 2026-10-19).
        return (ProfileInt(profile, "Level") + 1, ProfileInt(profile, "Credits"));
    }

    /// <summary>
    /// An int of ProfileData, found exactly once: u32 name length, the name, <c>" \0\0\0\0\x03\0\0\0"</c>,
    /// then the i32 (the Beta server's <c>profile_int</c>; the length keeps "Level" from matching
    /// "WristbandLevel").
    /// </summary>
    public static int? ProfileInt(ReadOnlySpan<byte> profile, string name)
    {
        ArgumentNullException.ThrowIfNull(name);
        var marker = new byte[4 + name.Length + 9];
        BitConverter.TryWriteBytes(marker.AsSpan(0, 4), (uint)name.Length);
        Encoding.ASCII.GetBytes(name, marker.AsSpan(4));
        " \0\0\0\0\x03\0\0\0"u8.CopyTo(marker.AsSpan(4 + name.Length));
        var at = profile.IndexOf(marker);
        if (at < 0 || profile[(at + 1)..].IndexOf(marker) >= 0 || at + marker.Length + 4 > profile.Length)
        {
            return null;
        }
        return BitConverter.ToInt32(profile.Slice(at + marker.Length, 4));
    }

    /// <summary>
    /// OD-41: makes <paramref name="source"/>'s newest sealed revision the newest revision of the
    /// Infinity profile's container <c>User_&lt;targetXuid&gt;</c>. Only while the game is not running
    /// (the caller checks). The source folder is only read.
    /// <list type="number">
    /// <item>Copy the picked revision to the archive (<c>picked</c>), and every revision of the target
    /// (<c>shadowed</c>): the clone shadows them and Azimuth prunes them after the next session.</item>
    /// <item>Stage a copy in <c>&lt;saves&gt;\fi-launcher-incoming</c> (same volume) and verify it against its commit.</item>
    /// <item>Rename it to <c>revisions\&lt;highest + 1, 16 lowercase hex&gt;</c>; it must then be the newest sealed revision.</item>
    /// </list>
    /// A crash leaves the target as it was plus, at most, a staging folder outside <c>revisions</c>.
    /// </summary>
    public static CloneResult Clone(string savesRoot, SaveProfile source, ulong targetXuid, DateTime nowUtc)
    {
        ArgumentNullException.ThrowIfNull(savesRoot);
        ArgumentNullException.ThrowIfNull(source);
        var target = FolderFor(targetXuid);
        if (string.Equals(source.Folder, target, StringComparison.OrdinalIgnoreCase))
        {
            throw new SaveException("a profile cannot be cloned onto itself");
        }
        var revision = ReadRevision(source.Revision.Number, source.Revision.Path)
            ?? throw new SaveException(source.Folder + ": the picked revision has no commit file any more");
        if (Verify(revision) is { Count: > 0 } changed)
        {
            throw new SaveException(source.Folder + ": " + string.Join("; ", changed));
        }
        var stamp = nowUtc.ToString("yyyyMMdd'T'HHmmss'Z'", CultureInfo.InvariantCulture);
        var backups = new List<string> { CopyDir(revision.Path, ArchiveSlot(savesRoot, source.Folder, $"picked-{stamp}", revision.Number)) };
        var targetDir = Path.Combine(savesRoot, "player-1", target);
        foreach (var (number, path) in RevisionFolders(targetDir))
        {
            backups.Add(CopyDir(path, ArchiveSlot(savesRoot, target, $"shadowed-{stamp}", number)));
        }

        var staging = Path.Combine(savesRoot, IncomingDir, $"{target}-{stamp}");
        if (Directory.Exists(staging))
        {
            Directory.Delete(staging, recursive: true);
        }
        string final;
        ulong next;
        try
        {
            Directory.CreateDirectory(Path.Combine(staging, "data"));
            CopyDurably(Path.Combine(revision.Path, "commit"), Path.Combine(staging, "commit"));
            foreach (var entry in revision.Entries)
            {
                CopyDurably(Path.Combine(revision.Path, "data", entry.Name), Path.Combine(staging, "data", entry.Name));
            }
            var staged = ReadRevision(0, staging) ?? throw new SaveException("the staged commit file does not parse");
            if (Verify(staged) is { Count: > 0 } torn)
            {
                throw new SaveException("the staged copy does not verify: " + string.Join("; ", torn));
            }
            next = NextRevisionNumber(targetDir);
            var revisions = Path.Combine(targetDir, "revisions");
            Directory.CreateDirectory(revisions);
            final = Path.Combine(revisions, next.ToString("x16", CultureInfo.InvariantCulture));
            Directory.Move(staging, final);
        }
        catch
        {
            // Only the launcher's own staging copy is removed; the target is as it was.
            try
            {
                if (Directory.Exists(staging))
                {
                    Directory.Delete(staging, recursive: true);
                }
            }
            catch (IOException)
            {
            }
            throw;
        }
        if (NewestSealed(targetDir) is not { } newest || newest.Number != next)
        {
            throw new SaveException("the cloned revision is not the newest sealed one", landed: true);
        }
        TryDeleteEmpty(Path.Combine(savesRoot, IncomingDir));
        return new CloneResult(target, next, final, backups);
    }

    internal static string ArchiveSlot(string savesRoot, string folder, string label, ulong number)
    {
        var name = $"{label}-{number.ToString("x16", CultureInfo.InvariantCulture)}";
        var root = Path.Combine(savesRoot, ArchiveDir, folder);
        var slot = Path.Combine(root, name);
        for (var i = 1; Directory.Exists(slot); i++)
        {
            slot = Path.Combine(root, $"{name}-{i}");
        }
        return slot;
    }

    internal static string CopyDir(string from, string to)
    {
        Directory.CreateDirectory(to);
        foreach (var file in Directory.EnumerateFiles(from))
        {
            File.Copy(file, Path.Combine(to, Path.GetFileName(file)));
        }
        foreach (var dir in Directory.EnumerateDirectories(from))
        {
            CopyDir(dir, Path.Combine(to, Path.GetFileName(dir)));
        }
        return to;
    }

    /// <summary>A copy flushed to disk before the rename that publishes it (saves.rs <c>sync_all</c>).</summary>
    internal static void CopyDurably(string from, string to)
    {
        using var source = new FileStream(from, FileMode.Open, FileAccess.Read, FileShare.Read);
        using var target = new FileStream(to, FileMode.CreateNew, FileAccess.Write, FileShare.None);
        source.CopyTo(target);
        target.Flush(flushToDisk: true);
    }

    internal static void TryDeleteEmpty(string dir)
    {
        try
        {
            if (Directory.Exists(dir) && !Directory.EnumerateFileSystemEntries(dir).Any())
            {
                Directory.Delete(dir);
            }
        }
        catch (IOException)
        {
        }
    }
}

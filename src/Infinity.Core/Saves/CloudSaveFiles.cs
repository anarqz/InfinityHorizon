// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;

namespace Infinity.Core.Saves;

/// <summary>A local revision read for the cloud: its files (verified against its commit), bundle and content digest.</summary>
public sealed record PackedRevision(ulong Number, IReadOnlyList<SaveFile> Files, byte[] Bundle, string ContentSha256);

/// <summary>The result of <see cref="CloudSaveFiles.Install"/>: the folder number the revision got, and the copies of what it shadows.</summary>
public sealed record InstalledRevision(ulong Number, string Path, IReadOnlyList<string> Archived);

/// <summary>The result of <see cref="CloudSaveFiles.QuarantineUnsealed"/>: folders moved to the archive, and busy folders left in place.</summary>
public sealed record PreparedFolders(IReadOnlyList<string> Moved, IReadOnlyList<string> Busy);

/// <summary>
/// The PC side of cloud saves on Azimuth's folders (<c>crates/fi-launcher-core/src/saves.rs</c>:
/// <c>LocalRevision::files/pack</c>, <c>install_files</c>, <c>quarantine_unsealed</c>,
/// <c>clean_incoming</c>). Nothing here deletes a revision: what an install shadows is copied to
/// <c>&lt;saves&gt;\fi-launcher-archive</c> first, and unsealed folders are moved there, not removed.
/// </summary>
public static class CloudSaveFiles
{
    /// <summary><c>&lt;saves&gt;\player-1\User_&lt;XUID upper hex&gt;</c>.</summary>
    public static string ProfileDir(string savesRoot, ulong xuid)
    {
        ArgumentNullException.ThrowIfNull(savesRoot);
        return Path.Combine(savesRoot, "player-1", AzimuthSaves.FolderFor(xuid));
    }

    /// <summary>
    /// The revision as bundle files in <see cref="Fh2Bundle.SaveFiles"/> order: the commit bytes as
    /// stored, then each known data file it lists. Each file is checked against its commit line on the
    /// bytes read (so a revision Azimuth rewrites meanwhile is never packed torn); an unknown name is
    /// refused, as the server would refuse it.
    /// </summary>
    public static IReadOnlyList<SaveFile> ReadFiles(LocalRevision revision)
    {
        ArgumentNullException.ThrowIfNull(revision);
        var commit = File.ReadAllBytes(Path.Combine(revision.Path, "commit"));
        var entries = AzimuthSaves.ParseCommit(new System.Text.UTF8Encoding(false, true).GetString(commit))
            ?? throw new SaveException("the revision's commit file does not parse any more");
        var known = Fh2Bundle.SaveFiles.Skip(1).Select(n => n[5..]).ToHashSet(StringComparer.Ordinal);
        if (entries.FirstOrDefault(e => !known.Contains(e.Name)) is { } unknown)
        {
            throw new SaveException(unknown.Name + " is not a save file");
        }
        var files = new List<SaveFile> { new("commit", commit) };
        foreach (var name in Fh2Bundle.SaveFiles.Skip(1))
        {
            if (entries.FirstOrDefault(e => e.Name == name[5..]) is not { } entry)
            {
                continue;
            }
            var bytes = File.ReadAllBytes(Path.Combine(revision.Path, "data", entry.Name));
            var line = Fh2Bundle.CommitLine(name, bytes);
            if (line != string.Create(CultureInfo.InvariantCulture, $"{entry.Sha256} {entry.Size} {entry.Name}"))
            {
                throw new SaveException(entry.Name + " does not match its commit line (the revision is not sealed)");
            }
            files.Add(new SaveFile(name, bytes));
        }
        return files;
    }

    /// <summary>Reads, packs (labelled with Azimuth's folder number) and digests a sealed revision.</summary>
    public static PackedRevision Pack(LocalRevision revision)
    {
        ArgumentNullException.ThrowIfNull(revision);
        var files = ReadFiles(revision);
        var bundle = Fh2Bundle.Pack(revision.Number, files);
        if (bundle.Length > Fh2Bundle.MaxBundle)
        {
            throw new SaveException($"the revision packs to {bundle.Length} bytes, over the {Fh2Bundle.MaxBundle} limit");
        }
        return new PackedRevision(revision.Number, files, bundle, Fh2Bundle.ContentDigest(files));
    }

    /// <summary>
    /// Moves every 16-hex folder above the newest sealed revision (all of them when none is sealed)
    /// into the archive, labelled <c>unsealed</c>; run before every reconcile and launch, so the highest
    /// folder Azimuth can load is the sealed one the decision was made on. A folder with a file open for
    /// writing (a sharing or lock violation) is not torn but busy: it stays and is reported in
    /// <see cref="PreparedFolders.Busy"/>, and the caller must not decide or launch until it settles.
    /// </summary>
    public static PreparedFolders QuarantineUnsealed(string savesRoot, ulong xuid)
    {
        var profileDir = ProfileDir(savesRoot, xuid);
        var sealedNumber = AzimuthSaves.NewestSealed(profileDir)?.Number;
        var moved = new List<string>();
        var busy = new List<string>();
        foreach (var (number, path) in AzimuthSaves.RevisionFolders(profileDir))
        {
            if (sealedNumber is { } s && number <= s)
            {
                continue;
            }
            if (AzimuthSaves.IsBusyFolder(path))
            {
                busy.Add(path);
                continue;
            }
            var dest = AzimuthSaves.ArchiveSlot(savesRoot, AzimuthSaves.FolderFor(xuid), "unsealed", number);
            Directory.CreateDirectory(Path.GetDirectoryName(dest)!);
            Directory.Move(path, dest);
            moved.Add(dest);
        }
        return new PreparedFolders(moved, busy);
    }

    /// <summary>The 16-hex folders above <paramref name="sealedNumber"/> (all when null) that someone is writing.</summary>
    public static IReadOnlyList<string> BusyAbove(string profileDir, ulong? sealedNumber) =>
        [.. AzimuthSaves.RevisionFolders(profileDir)
            .Where(f => sealedNumber is not { } s || f.Number > s)
            .Where(f => AzimuthSaves.IsBusyFolder(f.Path))
            .Select(f => f.Path)];

    /// <summary>Removes leftover staging folders of a crashed install (they were never in <c>revisions</c>).</summary>
    public static void CleanIncoming(string savesRoot)
    {
        ArgumentNullException.ThrowIfNull(savesRoot);
        var dir = Path.Combine(savesRoot, AzimuthSaves.IncomingDir);
        if (Directory.Exists(dir))
        {
            Directory.Delete(dir, recursive: true);
        }
    }

    /// <summary>
    /// Installs a downloaded, validated revision as the profile's highest revision (saves.rs
    /// <c>install_files</c>). Only while the game is not running (the caller probes).
    /// <list type="number">
    /// <item>The files' content digest must equal <paramref name="expectedContent"/> (the head's).</item>
    /// <item>Stage under <c>&lt;saves&gt;\fi-launcher-incoming\User_x-&lt;id&gt;</c> (same volume), flushed, and verify against the commit.</item>
    /// <item>Copy every existing 16-hex revision folder to the archive (<paramref name="label"/>), torn and commit-less ones too (as <see cref="AzimuthSaves.Clone"/> does): the install shadows them.</item>
    /// <item>Rename the staging folder to <c>revisions\&lt;highest 16-hex folder + 1&gt;</c>; it must then be the newest sealed one, with that digest.</item>
    /// </list>
    /// A crash leaves the old revisions in place and, at most, a staging folder outside <c>revisions</c>.
    /// A failed final check throws a <see cref="SaveException"/> with <see cref="SaveException.LandedPath"/>.
    /// </summary>
    public static InstalledRevision Install(string savesRoot, ulong xuid, IReadOnlyList<SaveFile> files, string expectedContent, string stagingId, string label) =>
        Install(savesRoot, xuid, files, expectedContent, stagingId, label, afterMove: null);

    /// <param name="afterMove">Test seam: runs right after the rename into <c>revisions</c>, before the final check.</param>
    internal static InstalledRevision Install(string savesRoot, ulong xuid, IReadOnlyList<SaveFile> files, string expectedContent, string stagingId, string label, Action<string>? afterMove)
    {
        ArgumentNullException.ThrowIfNull(savesRoot);
        ArgumentNullException.ThrowIfNull(files);
        ArgumentNullException.ThrowIfNull(stagingId);
        if (!string.Equals(Fh2Bundle.ContentDigest(files), expectedContent, StringComparison.OrdinalIgnoreCase))
        {
            throw new SaveException("the downloaded save does not match the cloud head's digest");
        }
        if (files.Count == 0 || files[0].Name != "commit")
        {
            throw new SaveException("the downloaded save has no commit file");
        }
        if (stagingId.Length == 0 || !stagingId.All(c => char.IsAsciiLetterOrDigit(c) || c == '-'))
        {
            throw new ArgumentException("a staging id is letters, digits and '-'", nameof(stagingId));
        }
        var folder = AzimuthSaves.FolderFor(xuid);
        var profileDir = ProfileDir(savesRoot, xuid);
        var staging = Path.Combine(savesRoot, AzimuthSaves.IncomingDir, $"{folder}-{stagingId}");
        if (Directory.Exists(staging))
        {
            Directory.Delete(staging, recursive: true);
        }
        var archived = new List<string>();
        ulong number;
        string target;
        try
        {
            Directory.CreateDirectory(Path.Combine(staging, "data"));
            foreach (var file in files)
            {
                using var stream = new FileStream(Path.Combine(staging, file.Name.Replace('/', Path.DirectorySeparatorChar)), FileMode.CreateNew, FileAccess.Write, FileShare.None);
                stream.Write(file.Bytes);
                stream.Flush(flushToDisk: true);
            }
            var staged = AzimuthSaves.ReadRevision(0, staging) ?? throw new SaveException("the downloaded commit file does not parse");
            if (AzimuthSaves.Verify(staged) is { Count: > 0 } problems)
            {
                throw new SaveException(string.Join("; ", problems));
            }
            foreach (var (n, path) in AzimuthSaves.RevisionFolders(profileDir))
            {
                // Every folder the install shadows, whether its commit parses or not: Azimuth prunes them later.
                archived.Add(AzimuthSaves.CopyDir(path, AzimuthSaves.ArchiveSlot(savesRoot, folder, label, n)));
            }
            number = AzimuthSaves.NextRevisionNumber(profileDir);
            var revisions = Path.Combine(profileDir, "revisions");
            Directory.CreateDirectory(revisions);
            target = Path.Combine(revisions, number.ToString("x16", CultureInfo.InvariantCulture));
            Directory.Move(staging, target);
        }
        catch
        {
            // Only the launcher's own staging copy is removed; the profile is as it was.
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
        afterMove?.Invoke(target);
        bool verified;
        try
        {
            verified = AzimuthSaves.NewestSealed(profileDir) is { } newest && newest.Number == number
                && string.Equals(Fh2Bundle.ContentDigest(ReadFiles(newest)), expectedContent, StringComparison.OrdinalIgnoreCase);
        }
        catch (Exception e) when (e is SaveException or IOException or UnauthorizedAccessException or System.Text.DecoderFallbackException)
        {
            throw new SaveException("the installed revision does not read back: " + e.Message, landed: true, landedPath: target);
        }
        if (!verified)
        {
            throw new SaveException("the installed revision is not the newest sealed one", landed: true, landedPath: target);
        }
        AzimuthSaves.TryDeleteEmpty(Path.Combine(savesRoot, AzimuthSaves.IncomingDir));
        return new InstalledRevision(number, target, archived);
    }
}

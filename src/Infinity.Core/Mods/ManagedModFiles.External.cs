// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.IO.Compression;
using System.Security.Cryptography;
using System.Text.Json;

namespace Infinity.Core.Mods;

/// <summary>One file an external-source mod expects: its source, path inside it, SHA-256 and size (OD-117).</summary>
public sealed record ExternalFile(string SourceId, string Path, string Sha256, long Bytes);

public enum ExternalFileState { Ok, Missing, Different }

public sealed record ExternalFileCheck(ExternalFile File, ExternalFileState State);

/// <summary>What a picked folder, file or ZIP held for one source: where the layout matched and each expected file's state.</summary>
public sealed record ExternalSourceCheck(string SourceId, string Picked, string? MatchedRoot, IReadOnlyList<ExternalFileCheck> Files)
{
    public int Found => Files.Count(f => f.State == ExternalFileState.Ok);
    public int Missing => Files.Count(f => f.State == ExternalFileState.Missing);
    public int Different => Files.Count(f => f.State == ExternalFileState.Different);
    /// <summary>Files this pick itself provided (the rest, if Ok, are an earlier verified copy of an update's unchanged files).</summary>
    public int FromSource { get; init; }
    /// <summary>
    /// Files this pick holds at their place but with other bytes (another version), which the earlier stored copy covers: the install
    /// uses the stored bytes, and the player is told (T2b; SAURON's Proton pass). Never silently ignored.
    /// </summary>
    public IReadOnlyList<ExternalFile> DifferentInPick { get; init; } = [];
    /// <summary>Every file verifies, and the pick provided at least one: a folder without any of the files is never "found".</summary>
    public bool Ok => Files.Count > 0 && Found == Files.Count && FromSource > 0;
}

/// <summary>Checking progress: files and bytes looked at, of the totals.</summary>
public readonly record struct ExternalProgress(int Files, int TotalFiles, long Bytes, long TotalBytes);

/// <summary>
/// OD-117: mods made of the player's own files (a car pack they already have). The catalog entry lists <c>externalSources</c>;
/// the player picks a folder (the pack's top or any parent), a file or a ZIP for each; the launcher finds the expected layout
/// inside it (by relative path, ignoring letter case, then by size and SHA-256 for renamed files), checks every file against
/// its pin and COPIES each verified file into the backup object store, so the source may move or vanish afterwards. The mod
/// then installs from the store like any package, into its one ExtraOptions folder (OD-232). No link is ever made; nothing is
/// distributed (Absolute rule 3).
/// </summary>
public sealed partial class ManagedModFiles
{
    private const int MaxSourceEntries = 50_000;
    /// <summary>Stable reasons a chosen source cannot be used (the <see cref="ModException"/> message; the launcher shows external.reason.&lt;reason&gt;).</summary>
    public const string SourceGone = "source_gone", SourceTooLarge = "source_too_large", SourceNotZip = "source_not_zip", SourceLinked = "source_linked";
    private const int MaxSourceDepth = 16;

    /// <summary>Every file an external mod reads from its sources: its mappings.</summary>
    public static IReadOnlyList<ExternalFile> ExternalFiles(ModManifest manifest)
    {
        ArgumentNullException.ThrowIfNull(manifest);
        if (!manifest.IsExternal) return [];
        static ExternalFile Of(string source, string sha256, long bytes) => new(ModManifest.ExternalSourceId(source), source[(source.IndexOf(':') + 1)..], sha256, bytes);
        return manifest.Files.Select(f => Of(f.Source, f.Hash.ToLowerInvariant(), f.Bytes))
            .DistinctBy(f => (f.SourceId, f.Path.ToLowerInvariant(), f.Sha256)).ToArray();
    }

    /// <summary>The expected files the store does not hold yet (all of them on first use; only changed ones for an update).</summary>
    public IReadOnlyList<ExternalFile> ExternalFilesNotStored(ModManifest manifest) =>
        ExternalFiles(manifest).Where(f => !ObjectStored(f.Sha256, f.Bytes)).ToArray();

    private bool ObjectStored(string digest, long bytes)
    {
        var path = Safe(BackupRoot, "objects/" + digest);
        return File.Exists(path) && new FileInfo(path).Length == bytes;
    }

    /// <summary>The stored copy's bytes still hash to its pin; a damaged one is removed so a fresh pick can store it again.</summary>
    private bool ObjectVerified(string digest, long bytes)
    {
        var path = Safe(BackupRoot, "objects/" + digest);
        if (!File.Exists(path)) return false;
        if (new FileInfo(path).Length == bytes)
            using (var stream = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.Read, 81920, FileOptions.SequentialScan))
                if (Convert.ToHexStringLower(SHA256.HashData(stream)) == digest) return true;
        RemoveFile(path);
        return false;
    }

    /// <summary>
    /// "Check files again" for an installed external mod: every stored copy is hashed (damaged ones removed); the files that no longer
    /// verify, which a new pick must provide. Reads only the store.
    /// </summary>
    public IReadOnlyList<ExternalFile> VerifyStoredExternal(ModManifest manifest, IProgress<ExternalProgress>? progress, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(manifest);
        using var gate = Lock();
        var files = ExternalFiles(manifest); var failed = new List<ExternalFile>(); long done = 0; var total = files.Sum(f => f.Bytes); var count = 0;
        foreach (var file in files)
        {
            cancellation.ThrowIfCancellationRequested();
            if (!ObjectVerified(file.Sha256, file.Bytes)) failed.Add(file);
            done += file.Bytes; progress?.Report(new(++count, files.Count, done, total));
        }
        return failed.AsReadOnly();
    }

    /// <summary>
    /// The checked files of a mod not installed yet, as a top-level record of the backup folder: retention (<see cref="Prune"/>) keeps
    /// every object a record names, so a restart between the check and Install never throws the copies away.
    /// </summary>
    private void RememberCheckedObjects(ModManifest manifest)
    {
        var digests = ExternalFiles(manifest).Where(f => ObjectStored(f.Sha256, f.Bytes)).Select(f => f.Sha256).ToHashSet(StringComparer.Ordinal);
        var path = Safe(BackupRoot, "external-" + manifest.Id + ".json");
        try
        {
            if (File.Exists(path) && JsonSerializer.Deserialize<CheckedObjects>(File.ReadAllBytes(path)) is { } earlier) digests.UnionWith(earlier.Objects);
        }
        catch (JsonException) { }
        Atomic(path, JsonSerializer.SerializeToUtf8Bytes(new CheckedObjects(manifest.Id, digests.Order(StringComparer.Ordinal).ToList()), Json));
    }

    private sealed record CheckedObjects(string Id, List<string> Objects);

    /// <summary>
    /// Looks for <paramref name="sourceId"/>'s files in <paramref name="picked"/> (a folder or a file; a <c>.zip</c> is read as an
    /// archive), checks each found file's size and SHA-256 and stores every verified one. Reads only from the source.
    /// </summary>
    public ExternalSourceCheck CheckExternalSource(ModManifest manifest, string sourceId, string picked, IProgress<ExternalProgress>? progress, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(manifest); ArgumentNullException.ThrowIfNull(picked);
        if (manifest.ExternalSources.All(s => s.Id != sourceId)) throw new ModException("unknown external source", ModErrors.Unsupported);
        using var gate = Lock(); // retention never prunes objects while they are being checked and stored
        using var source = SourceEntries.Open(picked);
        return Check(manifest, sourceId, picked, source, progress, cancellation);
    }

    /// <summary>The check of one source against an opened pick (the caller holds the lock).</summary>
    private ExternalSourceCheck Check(ModManifest manifest, string sourceId, string picked, SourceEntries source, IProgress<ExternalProgress>? progress, CancellationToken cancellation)
    {
        var expected = ExternalFiles(manifest).Where(f => f.SourceId == sourceId).ToArray();
        var (prefix, matched) = Match(source.Entries, expected);
        var bySize = source.Entries.ToLookup(e => e.Bytes);
        var results = new List<ExternalFileCheck>(); long done = 0; var total = expected.Sum(f => f.Bytes); var fromSource = 0;
        var differentInPick = new List<ExternalFile>();
        foreach (var file in expected)
        {
            cancellation.ThrowIfCancellationRequested();
            var state = ExternalFileState.Missing;
            // Every pick is checked against itself: a file is Ok when this pick holds it (by its path, else renamed: same size, same bytes).
            if (matched.TryGetValue(file, out var entry))
                state = entry.Bytes == file.Bytes && StoreIfMatches(entry, file) ? ExternalFileState.Ok : ExternalFileState.Different;
            if (state != ExternalFileState.Ok)
                foreach (var candidate in bySize[file.Bytes])
                    if (!ReferenceEquals(candidate, entry) && StoreIfMatches(candidate, file)) { state = ExternalFileState.Ok; break; }
            if (state == ExternalFileState.Ok) fromSource++;
            // An update's unchanged file may come from the earlier copy, only while its stored bytes still verify.
            else if (ObjectVerified(file.Sha256, file.Bytes))
            {
                if (state == ExternalFileState.Different) differentInPick.Add(file); // this pick's copy is another version: reported
                state = ExternalFileState.Ok;
            }
            results.Add(new(file, state));
            done += file.Bytes;
            progress?.Report(new(results.Count, expected.Length, done, total));
        }
        RememberCheckedObjects(manifest);
        if (fromSource > 0) RememberExternalChoice(manifest.Id, sourceId, picked); // under the lock: two rows never lose each other's pick
        var root = prefix is null ? null : source.Describe(prefix, matched.Values);
        return new(sourceId, picked, root, results.AsReadOnly()) { FromSource = fromSource, DifferentInPick = differentInPick.AsReadOnly() };
    }

    private bool StoreIfMatches(SourceEntry entry, ExternalFile file)
    {
        if (entry.Bytes != file.Bytes) return false;
        byte[] bytes;
        try { bytes = entry.Read(file.Bytes); }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException or InvalidDataException or NotSupportedException) { return false; }
        if (bytes.LongLength != file.Bytes || ModManifest.Hash(bytes) != file.Sha256) return false;
        _ = ObjectVerified(file.Sha256, file.Bytes); // a damaged earlier copy goes, so the good bytes can take its place
        if (Store(bytes) != file.Sha256) throw new ModException("stored file changed", ModErrors.Digest);
        return true;
    }

    /// <summary>
    /// The pack's root inside the pick: the prefix under which most expected paths exist (ignoring case), and each expected
    /// file found there. A pick that is the root itself has the empty prefix.
    /// </summary>
    private static (string? Prefix, Dictionary<ExternalFile, SourceEntry> Matched) Match(IReadOnlyList<SourceEntry> entries, IReadOnlyList<ExternalFile> expected)
    {
        var byName = expected.ToLookup(f => FileName(f.Path), StringComparer.OrdinalIgnoreCase);
        var votes = new Dictionary<string, int>(StringComparer.OrdinalIgnoreCase);
        foreach (var entry in entries)
            foreach (var file in byName[FileName(entry.Path)])
                if (entry.Path.Length >= file.Path.Length && entry.Path.EndsWith(file.Path, StringComparison.OrdinalIgnoreCase)
                    && (entry.Path.Length == file.Path.Length || entry.Path[entry.Path.Length - file.Path.Length - 1] == '/'))
                {
                    var prefix = entry.Path[..^file.Path.Length];
                    votes[prefix] = votes.GetValueOrDefault(prefix) + 1;
                }
        var matched = new Dictionary<ExternalFile, SourceEntry>();
        if (votes.Count == 0) return (null, matched);
        var best = votes.OrderByDescending(v => v.Value).ThenBy(v => v.Key.Length).First().Key;
        var index = new Dictionary<string, SourceEntry>(StringComparer.OrdinalIgnoreCase);
        foreach (var entry in entries) index.TryAdd(entry.Path, entry);
        foreach (var file in expected)
            if (index.TryGetValue(best + file.Path, out var entry)) matched[file] = entry;
        return (best, matched);
    }

    private static string FileName(string path) => path[(path.LastIndexOf('/') + 1)..];

    /// <summary>
    /// What Install would replace: the mapped destinations that already hold a file the launcher does not own for this mod (another
    /// mod's or the game's own), relative to their root, for the player to see before Install. Reads only.
    /// </summary>
    public IReadOnlyList<string> ExistingFilesReplaced(ModManifest manifest)
    {
        ArgumentNullException.ThrowIfNull(manifest);
        var owned = ReadReceipt(manifest.Id)?.Files.Select(f => f.Root + "/" + f.Path).ToHashSet(StringComparer.OrdinalIgnoreCase) ?? [];
        var replaced = new List<string>();
        foreach (var file in manifest.Files)
        {
            if (file.Root == "game" && _game is null || owned.Contains(file.Root + "/" + file.Destination)) continue;
            if (File.Exists(Target(file.Root, file.Destination))) replaced.Add(file.Destination);
        }
        return replaced.AsReadOnly();
    }

    /// <summary>
    /// OD-117 install: every expected file must already be in the store (checked when copied, checked again when read). Same
    /// transaction as a package: backups of what it replaces, receipts, settings.
    /// </summary>
    public void InstallExternal(ModManifest manifest, IProgress<string>? progress = null, bool update = false, bool hardcore = false)
    {
        ArgumentNullException.ThrowIfNull(manifest);
        if (!manifest.IsExternal) throw new ModException("not a mod of external files", ModErrors.Unsupported);
        if (!update) CheckProfile(manifest, hardcore);
        InstallFrom(manifest, () => VerifyExternalFiles(manifest), progress, update, hardcore);
    }

    private Dictionary<ManifestFile, Func<byte[]>> VerifyExternalFiles(ModManifest manifest)
    {
        if (ExternalFilesNotStored(manifest) is { Count: > 0 } missing)
            throw new ModException(missing.Count + " external files are not checked yet", ModErrors.ExternalFiles);
        var verified = new Dictionary<ManifestFile, Func<byte[]>>();
        foreach (var file in manifest.Files)
        {
            var digest = file.Hash.ToLowerInvariant();
            verified.Add(file, () => ObjectBytes(digest) is var bytes && bytes.LongLength == file.Bytes ? bytes : throw new ModException("stored file damaged", ModErrors.Digest));
        }
        return verified;
    }

    private string ExternalChoicesPath => Path.Combine(_state, "mods", "external-sources.json");

    /// <summary>The paths the player picked (mod id → source id → path), for re-check and repair; a path that no longer exists stays listed.</summary>
    public IReadOnlyDictionary<string, string> ExternalChoices(string modId)
    {
        try
        {
            var path = ExternalChoicesPath;
            if (!File.Exists(path) || new FileInfo(path).Length > 256 * 1024) return new Dictionary<string, string>();
            RejectLinks(path);
            var all = JsonSerializer.Deserialize<Dictionary<string, Dictionary<string, string>>>(File.ReadAllBytes(path));
            return all?.GetValueOrDefault(modId) ?? new Dictionary<string, string>();
        }
        catch (JsonException) { return new Dictionary<string, string>(); }
    }

    public void RememberExternalChoice(string modId, string sourceId, string picked)
    {
        var path = ExternalChoicesPath;
        Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        RejectLinks(path);
        Dictionary<string, Dictionary<string, string>> all;
        try { all = File.Exists(path) ? JsonSerializer.Deserialize<Dictionary<string, Dictionary<string, string>>>(File.ReadAllBytes(path)) ?? [] : []; }
        catch (JsonException) { all = []; }
        if (!all.TryGetValue(modId, out var sources)) all[modId] = sources = [];
        sources[sourceId] = Path.GetFullPath(picked);
        Atomic(path, JsonSerializer.SerializeToUtf8Bytes(all, Json));
    }

    /// <summary>At most this many ZIPs inside a picked folder are read as packs (one level: a ZIP inside one of them is only a file).</summary>
    internal const int MaxNestedZips = 256;

    /// <summary>
    /// A picked source's files: a folder walked without following links, one ZIP's entries, or one plain file. A folder's ZIPs (a folder
    /// of pack ZIPs, OD-232) are also read one level deep: each entry is addressed as if the ZIP were extracted beside it
    /// (<c>sub/Pack.zip</c> holding <c>Pack/mod.json</c> gives <c>sub/Pack/mod.json</c>), so <c>pack:Pack/…</c> matches inside any ZIP whose
    /// top folder is <c>Pack</c>. A ZIP inside a mod folder (one with a <c>mod.json</c> above it) is that mod's own file and is
    /// not opened; nor is a ZIP that cannot be read (it stays a plain file). Entries with a link, an absolute path or <c>..</c> are left out.
    /// </summary>
    private sealed class SourceEntries : IDisposable
    {
        private readonly ZipArchive? _zip;
        private readonly List<ZipArchive> _nested = [];
        private readonly string _picked;
        public IReadOnlyList<SourceEntry> Entries { get; private set; } = [];

        private SourceEntries(string picked, ZipArchive? zip, IReadOnlyList<SourceEntry> entries) { _picked = picked; _zip = zip; Entries = entries; }

        public static SourceEntries Open(string picked)
        {
            var full = Path.GetFullPath(picked);
            // The walk skips links inside the pick; the pick itself is not followed either (a symbolic link or junction: review, T2 M7).
            // LinkTarget is null for cloud placeholders (OneDrive), which are the player's own files and stay allowed.
            FileSystemInfo item = Directory.Exists(full) ? new DirectoryInfo(full) : new FileInfo(full);
            if (item.Exists && item.LinkTarget is not null) throw new ModException(SourceLinked, ModErrors.ExternalFiles);
            if (Directory.Exists(full))
            {
                var options = new EnumerationOptions { RecurseSubdirectories = true, MaxRecursionDepth = MaxSourceDepth, IgnoreInaccessible = true,
                    AttributesToSkip = FileAttributes.ReparsePoint | FileAttributes.System, ReturnSpecialDirectories = false };
                var entries = new List<SourceEntry>();
                var zips = new List<(string Relative, string Path)>();
                var modFolders = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
                foreach (var file in new DirectoryInfo(full).EnumerateFiles("*", options))
                {
                    if (entries.Count >= MaxSourceEntries) throw new ModException(SourceTooLarge, ModErrors.ExternalFiles);
                    var path = file.FullName;
                    var relative = Path.GetRelativePath(full, path).Replace('\\', '/');
                    entries.Add(new(relative, file.Length, limit => ReadFile(path, limit)));
                    if (file.Name.Equals("mod.json", StringComparison.OrdinalIgnoreCase)) modFolders.Add(Parent(relative));
                    else if (file.Extension.Equals(".zip", StringComparison.OrdinalIgnoreCase)) zips.Add((relative, path));
                }
                var source = new SourceEntries(full, null, entries);
                try
                {
                    // In walk order (stable), the ZIPs no mod folder holds: one level, at most MaxNestedZips of them.
                    foreach (var (relative, path) in zips.Where(z => !InsideModFolder(z.Relative, modFolders)))
                    {
                        if (source._nested.Count >= MaxNestedZips) throw new ModException(SourceTooLarge, ModErrors.ExternalFiles);
                        ZipArchive? zip = null;
                        try
                        {
                            zip = ZipFile.OpenRead(path);
                            if (entries.Count + zip.Entries.Count > MaxSourceEntries) throw new ModException(SourceTooLarge, ModErrors.ExternalFiles);
                            var beside = Parent(relative) is { Length: > 0 } folder ? folder + "/" : "";
                            var inner = zip.Entries.Where(e => !e.FullName.EndsWith('/') && !e.FullName.EndsWith('\\') && PlainEntry(e))
                                .Select(e => new SourceEntry(beside + e.FullName.Replace('\\', '/'), e.Length, limit => ReadEntry(e, limit)) { Container = path, Inside = beside })
                                .ToArray();
                            source._nested.Add(zip); zip = null;
                            entries.AddRange(inner);
                        }
                        // not a readable ZIP (or one the player's system keeps closed): only a file of the pick
                        catch (Exception error) when (error is InvalidDataException or IOException or UnauthorizedAccessException or NotSupportedException) { }
                        finally { zip?.Dispose(); }
                    }
                }
                catch { source.Dispose(); throw; }
                return source;
            }
            if (!File.Exists(full)) throw new ModException(SourceGone, ModErrors.ExternalFiles);
            if (full.EndsWith(".zip", StringComparison.OrdinalIgnoreCase))
            {
                ZipArchive? zip = null;
                try
                {
                    // A corrupt central directory surfaces only when the entries are read: it is the same "not a readable ZIP".
                    zip = ZipFile.OpenRead(full);
                    if (zip.Entries.Count > MaxSourceEntries) throw new ModException(SourceTooLarge, ModErrors.ExternalFiles);
                    var entries = zip.Entries.Where(e => !e.FullName.EndsWith('/') && !e.FullName.EndsWith('\\'))
                        .Select(e => new SourceEntry(e.FullName.Replace('\\', '/'), e.Length, limit => ReadEntry(e, limit))).ToArray();
                    return new(full, zip, entries);
                }
                catch (Exception error) when (error is InvalidDataException or ModException)
                {
                    zip?.Dispose();
                    throw error as ModException ?? new ModException(SourceNotZip, ModErrors.ExternalFiles);
                }
            }
            var single = new FileInfo(full);
            return new(Path.GetDirectoryName(full)!, null, [new(single.Name, single.Length, limit => ReadFile(full, limit))]);
        }

        /// <summary>The matched pack root, for the player: a folder path, or the ZIP with the folder inside it.</summary>
        public string Describe(string prefix, IEnumerable<SourceEntry> matched)
        {
            if (_zip is not null) return prefix.Length == 0 ? _picked : _picked + " › " + prefix.TrimEnd('/');
            // A pack read inside one of the folder's ZIPs: the ZIP most matched files came from, with the folder inside it.
            if (matched.GroupBy(e => e.Container).OrderByDescending(g => g.Count()).FirstOrDefault() is { Key: { } container } nested)
            {
                var inside = nested.First().Inside;
                var inner = prefix.Length > inside.Length ? prefix[inside.Length..].TrimEnd('/') : "";
                return inner.Length == 0 ? container : container + " › " + inner;
            }
            return Path.TrimEndingDirectorySeparator(Path.Combine(_picked, prefix.Replace('/', Path.DirectorySeparatorChar)));
        }

        public void Dispose()
        {
            _zip?.Dispose();
            foreach (var zip in _nested) zip.Dispose();
            _nested.Clear();
        }

        private static string Parent(string relative) => relative.LastIndexOf('/') is var slash and > 0 ? relative[..slash] : "";

        /// <summary>The ZIP sits in a folder of the pick (or the pick itself) that holds a <c>mod.json</c>: one mod's own file.</summary>
        private static bool InsideModFolder(string relative, HashSet<string> modFolders)
        {
            for (var folder = Parent(relative); ; folder = Parent(folder))
            {
                if (modFolders.Contains(folder)) return true;
                if (folder.Length == 0) return false;
            }
        }

        /// <summary>A nested entry the player's pack could hold: no link, and a relative path every manifest source could name.</summary>
        private static bool PlainEntry(ZipArchiveEntry entry)
        {
            if ((entry.ExternalAttributes >> 16 & 0xF000) == 0xA000 || (entry.ExternalAttributes & (int)FileAttributes.ReparsePoint) != 0) return false;
            try { ModManifest.ValidatePath(entry.FullName.Replace('\\', '/')); return true; }
            catch (ModException) { return false; }
        }

        private static byte[] ReadFile(string path, long limit)
        {
            using var stream = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.Read, 81920, FileOptions.SequentialScan);
            if (stream.Length != limit) return [];
            var bytes = new byte[checked((int)limit)]; stream.ReadExactly(bytes);
            return bytes;
        }

        private static byte[] ReadEntry(ZipArchiveEntry entry, long limit)
        {
            if (entry.Length != limit) return [];
            using var stream = entry.Open();
            var bytes = new byte[checked((int)limit)]; stream.ReadExactly(bytes);
            return stream.ReadByte() == -1 ? bytes : [];
        }
    }

    private sealed record SourceEntry(string Path, long Bytes, Func<long, byte[]> Read)
    {
        /// <summary>The folder's ZIP this entry was read from (one level, OD-232), or null.</summary>
        public string? Container { get; init; }
        /// <summary>The ZIP's own folder inside the pick (<c>sub/</c>; empty at the top) that prefixes the entry's path.</summary>
        public string Inside { get; init; } = "";
    }
}

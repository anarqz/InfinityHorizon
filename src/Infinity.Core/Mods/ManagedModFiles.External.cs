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
/// then installs from the store like any package. No link is ever made; nothing is distributed (Absolute rule 3).
/// </summary>
public sealed partial class ManagedModFiles
{
    private const int MaxSourceEntries = 50_000;
    /// <summary>Stable reasons a chosen source cannot be used (the <see cref="ModException"/> message; the launcher shows external.reason.&lt;reason&gt;).</summary>
    public const string SourceGone = "source_gone", SourceTooLarge = "source_too_large", SourceNotZip = "source_not_zip";
    private const int MaxSourceDepth = 16;

    /// <summary>Every file an external mod reads from its sources: its mappings and its database base.</summary>
    public static IReadOnlyList<ExternalFile> ExternalFiles(ModManifest manifest)
    {
        ArgumentNullException.ThrowIfNull(manifest);
        if (!manifest.IsExternal) return [];
        static ExternalFile Of(string source, string sha256, long bytes) => new(ModManifest.ExternalSourceId(source), source[(source.IndexOf(':') + 1)..], sha256, bytes);
        var files = manifest.Files.Select(f => Of(f.Source, f.Hash.ToLowerInvariant(), f.Bytes)).ToList();
        if (manifest.DatabaseBase is { } baseDb) files.Add(Of(baseDb.Path, baseDb.Sha256, baseDb.Bytes));
        return files.DistinctBy(f => (f.SourceId, f.Path.ToLowerInvariant(), f.Sha256)).ToArray();
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
        var expected = ExternalFiles(manifest).Where(f => f.SourceId == sourceId).ToArray();
        using var gate = Lock(); // retention never prunes objects while they are being checked and stored
        using var source = SourceEntries.Open(picked);
        var (prefix, matched) = Match(source.Entries, expected);
        var bySize = source.Entries.ToLookup(e => e.Bytes);
        var results = new List<ExternalFileCheck>(); long done = 0; var total = expected.Sum(f => f.Bytes); var fromSource = 0;
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
            else if (ObjectVerified(file.Sha256, file.Bytes)) state = ExternalFileState.Ok;
            results.Add(new(file, state));
            done += file.Bytes;
            progress?.Report(new(results.Count, expected.Length, done, total));
        }
        RememberCheckedObjects(manifest);
        if (fromSource > 0) RememberExternalChoice(manifest.Id, sourceId, picked); // under the lock: two rows never lose each other's pick
        var root = prefix is null ? null : source.Describe(prefix);
        return new(sourceId, picked, root, results.AsReadOnly()) { FromSource = fromSource };
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
    /// OD-117 install: every expected file must already be in the store (checked when copied, checked again when read). Same
    /// transaction as a package: backups of what it replaces, receipts, settings, one database base at a time (OD-114).
    /// </summary>
    public void InstallExternal(ModManifest manifest, IProgress<string>? progress = null, bool update = false, bool hardcore = false)
    {
        ArgumentNullException.ThrowIfNull(manifest);
        if (!manifest.IsExternal) throw new ModException("not a mod of external files", ModErrors.Unsupported);
        if (!update) CheckProfile(manifest, hardcore);
        if (manifest.DatabaseBase is not null && !DataSqlRuntimeAvailable()) throw new ModException("a database base needs the mod runtime", ModErrors.RuntimeRequired);
        CheckDatabaseBase(manifest);
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
        if (manifest.DatabaseBase is { } baseDb)
        {
            var bytes = ObjectBytes(baseDb.Sha256);
            if (bytes.LongLength != baseDb.Bytes || !StandaloneSqlite(bytes)) throw new ModException("database base must be a checkpointed rollback-journal SQLite file", ModErrors.Unsupported);
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

    /// <summary>A picked source's files: a folder walked without following links, one ZIP's entries, or one plain file.</summary>
    private sealed class SourceEntries : IDisposable
    {
        private readonly ZipArchive? _zip;
        private readonly string _picked;
        public IReadOnlyList<SourceEntry> Entries { get; }

        private SourceEntries(string picked, ZipArchive? zip, IReadOnlyList<SourceEntry> entries) { _picked = picked; _zip = zip; Entries = entries; }

        public static SourceEntries Open(string picked)
        {
            var full = Path.GetFullPath(picked);
            if (Directory.Exists(full))
            {
                var options = new EnumerationOptions { RecurseSubdirectories = true, MaxRecursionDepth = MaxSourceDepth, IgnoreInaccessible = true,
                    AttributesToSkip = FileAttributes.ReparsePoint | FileAttributes.System, ReturnSpecialDirectories = false };
                var entries = new List<SourceEntry>();
                foreach (var file in new DirectoryInfo(full).EnumerateFiles("*", options))
                {
                    if (entries.Count >= MaxSourceEntries) throw new ModException(SourceTooLarge, ModErrors.ExternalFiles);
                    var path = file.FullName;
                    entries.Add(new(Path.GetRelativePath(full, path).Replace('\\', '/'), file.Length, limit => ReadFile(path, limit)));
                }
                return new(full, null, entries);
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
        public string Describe(string prefix) => _zip is null
            ? Path.TrimEndingDirectorySeparator(Path.Combine(_picked, prefix.Replace('/', Path.DirectorySeparatorChar)))
            : prefix.Length == 0 ? _picked : _picked + " › " + prefix.TrimEnd('/');

        public void Dispose() => _zip?.Dispose();

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

    private sealed record SourceEntry(string Path, long Bytes, Func<long, byte[]> Read);
}

// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.IO.Compression;
using System.Net;
using System.Security.Cryptography;

namespace Infinity.Core.Mods;

/// <summary>A custom mod added from a ZIP: its manifest, and whether it replaced (or already matched) a folder of the same id.</summary>
public sealed record CustomModImport(ModManifest Manifest, bool Replaced, bool Unchanged);

/// <summary>
/// "Add from URL" (OD-114 follow-up): a ZIP holding one custom mod laid out as Infinity reads it (<c>infinity-mod.json</c>,
/// <c>files/</c>, <c>database/</c>), at the archive's root or inside any folder. The launcher finds the one manifest, checks every
/// file it maps against its size and SHA-256, and only then places the folder at <c>&lt;home&gt;/mods/custom/&lt;id&gt;</c>, named
/// by the manifest's own <c>custom.</c> id. Nothing in the archive but the manifest and its mapped files is written. Adding a mod
/// never confirms it: the player still confirms its exact digest on its card before any install.
/// </summary>
public sealed partial class ManagedModFiles
{
    public const long MaxCustomArchiveBytes = 2L * 1024 * 1024 * 1024;
    private const long MaxCustomExtractedBytes = 4L * 1024 * 1024 * 1024;
    private const int MaxCustomArchiveEntries = 20000;
    /// <summary>Earlier versions replaced by an import are moved here (skipped by the scan, never deleted by the launcher).</summary>
    public const string CustomReplacedFolder = ".replaced";

    /// <summary>Downloads a ZIP from a player-supplied HTTPS address (any host, at most five HTTPS redirects) and imports it.</summary>
    public static async Task<CustomModImport> ImportCustomFromUrlAsync(HttpClient http, string home, Uri url, IProgress<string>? progress, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(http); ArgumentNullException.ThrowIfNull(url);
        var root = CustomModsFolder(home);
        Directory.CreateDirectory(root);
        RejectLinks(root, Path.GetFullPath(home), Process.WineHost.Detected);
        RemoveImportLeftovers(root);
        var archive = Path.Combine(root, "." + Guid.NewGuid().ToString("N") + DownloadSuffix);
        try
        {
            progress?.Report("download");
            // A server that stops sending for a minute is a failed download, not a launcher stuck until the overall timeout.
            using var idle = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
            try
            {
                idle.CancelAfter(IdleTimeout);
                using var response = await ModDownloads.DownloadAsync(http, url, _ => true, idle.Token).ConfigureAwait(false);
                if (response.StatusCode != HttpStatusCode.OK) throw new ModException("custom mod download failed: " + (int)response.StatusCode, ModErrors.UrlDownload);
                if (response.Content.Headers.ContentLength > MaxCustomArchiveBytes) throw new ModException("custom mod archive too large", ModErrors.Archive);
                await using var body = await response.Content.ReadAsStreamAsync(idle.Token).ConfigureAwait(false);
                await using var file = new FileStream(archive, FileMode.CreateNew, FileAccess.Write, FileShare.None, 81920, useAsync: true);
                var buffer = new byte[81920]; long total = 0;
                for (int read; ;)
                {
                    idle.CancelAfter(IdleTimeout);
                    if ((read = await body.ReadAsync(buffer, idle.Token).ConfigureAwait(false)) == 0) break;
                    total += read;
                    if (total > MaxCustomArchiveBytes) throw new ModException("custom mod archive too large", ModErrors.Archive);
                    await file.WriteAsync(buffer.AsMemory(0, read), cancellation).ConfigureAwait(false);
                }
            }
            catch (ModException error) when (error.Code == ModErrors.Download) { throw new ModException(error.Message, ModErrors.UrlDownload); }
            catch (HttpRequestException error) { throw new ModException("custom mod download failed: " + error.HttpRequestError, ModErrors.UrlDownload); }
            // A connection dropped mid-body (HttpIOException), a stall or the overall timeout: all the address's fault, never the folder's.
            catch (HttpIOException) { throw new ModException("custom mod download interrupted", ModErrors.UrlDownload); }
            catch (OperationCanceledException) { throw new ModException("custom mod download timed out", ModErrors.UrlDownload); }
            progress?.Report("verify");
            return await Task.Run(() => ImportCustomArchive(home, archive), cancellation).ConfigureAwait(false);
        }
        finally { BestEffort(() => { if (File.Exists(archive)) RemoveFile(archive); }); }
    }

    private static readonly TimeSpan IdleTimeout = TimeSpan.FromSeconds(60);
    private const string DownloadSuffix = ".download.tmp";
    private const string ImportSuffix = ".import";

    /// <summary>Imports a local ZIP (see the type's remarks). The archive itself is only read.</summary>
    public static CustomModImport ImportCustomArchive(string home, string archivePath)
    {
        // An unsupported compression method (LZMA, BZip2, AES) or corrupt data surfaces only when an entry is opened or read.
        try { return ImportArchive(home, archivePath); }
        catch (Exception error) when (error is InvalidDataException or NotSupportedException)
        { throw new ModException("unreadable ZIP archive: " + error.GetType().Name, ModErrors.Archive); }
    }

    private static CustomModImport ImportArchive(string home, string archivePath)
    {
        var root = CustomModsFolder(home);
        Directory.CreateDirectory(root);
        var boundary = Path.GetFullPath(home);
        RejectLinks(root, boundary, Process.WineHost.Detected);
        using (var zip = ZipFile.OpenRead(archivePath))
        {
            if (zip.Entries.Count > MaxCustomArchiveEntries) throw new ModException("too many archive entries", ModErrors.Archive);
            // Keyed without letter case: two entries one Windows folder would merge are refused, never resolved by order.
            var entries = new Dictionary<string, ZipArchiveEntry>(StringComparer.OrdinalIgnoreCase);
            foreach (var entry in zip.Entries)
            {
                var name = entry.FullName.Replace('\\', '/');
                if (name.EndsWith('/') || name.StartsWith("__MACOSX/", StringComparison.OrdinalIgnoreCase)) continue;
                if (!entries.TryAdd(name, entry)) throw new ModException("duplicate archive entries", ModErrors.Archive);
            }

            var manifests = entries.Keys.Where(k => string.Equals(k.Split('/')[^1], CustomManifestName, StringComparison.OrdinalIgnoreCase)).ToArray();
            if (manifests.Length != 1) throw new ModException(manifests.Length == 0 ? "no manifest in the archive" : "several manifests in the archive", ModErrors.ArchiveManifest);
            var prefix = manifests[0][..^CustomManifestName.Length];
            var manifestEntry = entries[manifests[0]];
            if (manifestEntry.Length > ModCatalogTrust.MaxManifestBytes) throw new ModException("custom mod manifest too large", ModErrors.ArchiveManifest);
            var manifestBytes = ReadEntry(manifestEntry, manifestEntry.Length);
            ModManifest manifest;
            try { manifest = new ModManifest(manifestBytes); }
            catch (Exception error) when (error is System.Text.Json.JsonException or KeyNotFoundException or InvalidOperationException or FormatException or ModException)
            { throw new ModException("invalid manifest in the archive: " + error.Message, ModErrors.ArchiveManifest); }
            if (!manifest.IsCustom) throw new ModException("the archive's manifest is not a custom mod", ModErrors.ArchiveManifest);

            // Exactly the files the manifest maps, at the paths the custom folder reads them from, with their pinned size and hash.
            var wanted = manifest.Files.Select(f => (Path: ModManifest.CustomPayloadFolder + "/" + f.Source, f.Bytes, f.Hash)).ToList();
            if (manifest.DatabaseSql is { } sql) wanted.Add((sql.Path, sql.Bytes, sql.Sha256));
            if (manifest.DatabaseBase is { } baseDb) wanted.Add((baseDb.Path, baseDb.Bytes, baseDb.Sha256));
            if (wanted.Sum(w => w.Bytes) > MaxCustomExtractedBytes) throw new ModException("custom mod too large", ModErrors.Archive);
            // The install compares lowercase hex exactly; an uppercase pin would be added here and then refused there.
            if (wanted.Any(w => w.Hash.Any(char.IsAsciiLetterUpper))) throw new ModException("custom mod hashes must be lowercase hex", ModErrors.ArchiveManifest);

            var target = Resolve(root, manifest.Id);
            RejectLinks(target, boundary, Process.WineHost.Detected);
            var existing = Path.Combine(target, CustomManifestName);
            if (Directory.Exists(target) && File.Exists(existing) && new FileInfo(existing).Length <= ModCatalogTrust.MaxManifestBytes
                && ModManifest.Hash(File.ReadAllBytes(existing)) == manifest.Digest && wanted.All(w => SameFile(Path.Combine(target, w.Path), w.Bytes, w.Hash)))
                return new(manifest, Replaced: false, Unchanged: true);
            if (!Directory.Exists(target) && ScanCustomMods(home).Mods.Count >= MaxCustomMods) throw new ModException("custom mods folder full", ModErrors.CustomFull);

            var staging = Path.Combine(root, "." + Guid.NewGuid().ToString("N") + ImportSuffix);
            Directory.CreateDirectory(staging);
            try
            {
                foreach (var (path, bytes, hash) in wanted)
                {
                    // Entry names written on Windows may differ in letter case from the manifest; the key ignores it.
                    var entry = entries.GetValueOrDefault(prefix + path) ?? throw new ModException("archive file missing: " + path, ModErrors.Archive);
                    if (entry.Length != bytes) throw new ModException("archive file size mismatch: " + path, ModErrors.Digest);
                    var destination = Resolve(staging, path);
                    Directory.CreateDirectory(Path.GetDirectoryName(destination)!);
                    using (var input = entry.Open())
                    using (var output = new FileStream(destination, FileMode.CreateNew, FileAccess.Write, FileShare.None))
                    using (var sha = IncrementalHash.CreateHash(HashAlgorithmName.SHA256))
                    {
                        var buffer = new byte[81920]; long total = 0;
                        for (int read; (read = input.Read(buffer)) > 0;)
                        {
                            total += read;
                            if (total > bytes) throw new ModException("archive file larger than declared: " + path, ModErrors.Digest);
                            sha.AppendData(buffer, 0, read); output.Write(buffer, 0, read);
                        }
                        if (total != bytes || Convert.ToHexStringLower(sha.GetHashAndReset()) != hash)
                            throw new ModException("archive file digest mismatch: " + path, ModErrors.Digest);
                    }
                }
                File.WriteAllBytes(Path.Combine(staging, CustomManifestName), manifestBytes);

                var replaced = Directory.Exists(target);
                if (replaced)
                {
                    // The earlier folder (possibly the player's own edits) is kept aside, never deleted.
                    var aside = Path.Combine(root, CustomReplacedFolder);
                    Directory.CreateDirectory(aside);
                    RejectLinks(aside, boundary, Process.WineHost.Detected);
                    Directory.Move(target, Path.Combine(aside, manifest.Id + "." + DateTimeOffset.UtcNow.ToString("yyyyMMddHHmmss", System.Globalization.CultureInfo.InvariantCulture) + "." + Guid.NewGuid().ToString("N")[..8]));
                }
                Directory.Move(staging, target);
                return new(manifest, replaced, Unchanged: false);
            }
            finally { BestEffort(() => { if (Directory.Exists(staging)) RemoveStaging(staging); }); }
        }
    }

    private static byte[] ReadEntry(ZipArchiveEntry entry, long length)
    {
        using var stream = entry.Open();
        var bytes = new byte[checked((int)length)];
        stream.ReadExactly(bytes);
        if (stream.ReadByte() != -1) throw new ModException("archive entry larger than declared", ModErrors.Archive);
        return bytes;
    }

    private static bool SameFile(string path, long bytes, string hash)
    {
        if (!File.Exists(path) || new FileInfo(path).Length != bytes) return false;
        using var stream = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.Read);
        return Convert.ToHexStringLower(SHA256.HashData(stream)) == hash;
    }

    /// <summary>Cleanup that must not replace the operation's own result or error (a scanner holding the fresh file); a leftover goes next time.</summary>
    private static void BestEffort(Action cleanup)
    {
        try { cleanup(); }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException) { }
    }

    /// <summary>
    /// A crashed import's downloads and staging folders (<c>.&lt;32 hex&gt;.download.tmp</c>, <c>.&lt;32 hex&gt;.import</c>) older than a day; a
    /// younger one may belong to another launcher window still importing.
    /// </summary>
    private static void RemoveImportLeftovers(string root)
    {
        static bool Ours(string name, string suffix) => name.StartsWith('.') && name.EndsWith(suffix, StringComparison.Ordinal)
            && name[1..^suffix.Length] is { Length: 32 } hex && hex.All(char.IsAsciiHexDigitLower);
        var old = DateTime.UtcNow - TimeSpan.FromDays(1);
        foreach (var file in Directory.EnumerateFiles(root).Where(f => Ours(Path.GetFileName(f), DownloadSuffix) && File.GetLastWriteTimeUtc(f) < old))
            BestEffort(() => RemoveFile(file));
        foreach (var folder in Directory.EnumerateDirectories(root).Where(d => Ours(Path.GetFileName(d), ImportSuffix) && Directory.GetLastWriteTimeUtc(d) < old && !IsLink(d)))
            BestEffort(() => RemoveStaging(folder));
    }

    /// <summary>Removes an import's own staging folder: only plain files and folders this import created.</summary>
    private static void RemoveStaging(string staging)
    {
        foreach (var file in Directory.EnumerateFiles(staging, "*", SearchOption.AllDirectories)) RemoveFile(file);
        foreach (var folder in Directory.EnumerateDirectories(staging, "*", SearchOption.AllDirectories).OrderByDescending(f => f.Length)) Directory.Delete(folder);
        Directory.Delete(staging);
    }
}

// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.IO.Compression;
using System.Net;
using System.Text.Json;

namespace Infinity.Core.Mods;

/// <summary>An OD-232 mod added from a ZIP: its id, name and version (from its <c>mod.json</c>) and the earlier folder of that id moved aside, or null.</summary>
public sealed record ExtraOptionsModImport(string Id, string Name, string? Version, string? ReplacedAside);

/// <summary>
/// "Add from URL" and a local ZIP (OD-232): an archive holding one ExtraOptions mod, its <c>mod.json</c> (format 1, a valid id) at
/// the archive's top or inside one top folder, is placed at <c>&lt;Azimuth&gt;\mods\&lt;game folder&gt;\&lt;id&gt;\</c> (OD-233) exactly as a mod copied there by
/// hand (no receipt: the manual-mods list shows it, and the loader enables it at the next start). Only the mod's folder is written
/// (entries outside it are ignored), only with the game closed, and only plain files: a link, an absolute path or <c>..</c> refuses the
/// archive. Everything is extracted to a staging folder beside the target first, then moved in one step; an existing folder of the
/// same id is moved to the game folder's <c>.replaced\</c>, never deleted. A folder of a mod the launcher installed is never replaced.
/// </summary>
public sealed partial class ManagedModFiles
{
    public const long MaxImportArchiveBytes = 2L * 1024 * 1024 * 1024;
    private const long MaxImportExtractedBytes = 4L * 1024 * 1024 * 1024;
    private const int MaxImportEntries = 20000;
    private const int MaxImportManifestBytes = 1024 * 1024;
    /// <summary>Earlier folders an import replaced, under <c>mods\</c> (a dot folder: the loader and the launcher skip it; never deleted by the launcher).</summary>
    public const string ReplacedFolder = ".replaced";
    private const string ModJson = "mod.json";

    /// <summary>Downloads a ZIP from a player-supplied HTTPS address (any host, at most five HTTPS redirects) and imports it.</summary>
    public async Task<ExtraOptionsModImport> ImportFromUrlAsync(HttpClient http, Uri url, IProgress<string>? progress, CancellationToken cancellation, string game = ModManifest.DefaultGame)
    {
        ArgumentNullException.ThrowIfNull(http); ArgumentNullException.ThrowIfNull(url);
        CheckGame();
        var root = ImportRoot(game);
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
                if (response.StatusCode != HttpStatusCode.OK) throw new ModException("mod download failed: " + (int)response.StatusCode, ModErrors.UrlDownload);
                if (response.Content.Headers.ContentLength > MaxImportArchiveBytes) throw new ModException("mod archive too large", ModErrors.Archive);
                await using var body = await response.Content.ReadAsStreamAsync(idle.Token).ConfigureAwait(false);
                await using var file = new FileStream(archive, FileMode.CreateNew, FileAccess.Write, FileShare.None, 81920, useAsync: true);
                var buffer = new byte[81920]; long total = 0;
                for (int read; ;)
                {
                    idle.CancelAfter(IdleTimeout);
                    if ((read = await body.ReadAsync(buffer, idle.Token).ConfigureAwait(false)) == 0) break;
                    total += read;
                    if (total > MaxImportArchiveBytes) throw new ModException("mod archive too large", ModErrors.Archive);
                    await file.WriteAsync(buffer.AsMemory(0, read), cancellation).ConfigureAwait(false);
                }
            }
            catch (ModException error) when (error.Code == ModErrors.Download) { throw new ModException(error.Message, ModErrors.UrlDownload); }
            catch (HttpRequestException error) { throw new ModException("mod download failed: " + error.HttpRequestError, ModErrors.UrlDownload); }
            // A connection dropped mid-body (HttpIOException), a stall or the overall timeout: all the address's fault, never the folder's.
            catch (HttpIOException) { throw new ModException("mod download interrupted", ModErrors.UrlDownload); }
            catch (OperationCanceledException) { throw new ModException("mod download timed out", ModErrors.UrlDownload); }
            progress?.Report("verify");
            return await Task.Run(() => ImportArchive(archive, game), cancellation).ConfigureAwait(false);
        }
        finally { BestEffort(() => { if (File.Exists(archive)) RemoveFile(archive); }); }
    }

    private static readonly TimeSpan IdleTimeout = TimeSpan.FromSeconds(60);
    private const string DownloadSuffix = ".download.tmp";
    private const string ImportSuffix = ".import";

    /// <summary>Imports a local ZIP (see the type's remarks). The archive itself is only read.</summary>
    public ExtraOptionsModImport ImportArchive(string archivePath, string game = ModManifest.DefaultGame)
    {
        ArgumentNullException.ThrowIfNull(archivePath);
        // An unsupported compression method (LZMA, BZip2, AES) or corrupt data surfaces only when an entry is opened or read.
        try { return Import(archivePath, game); }
        catch (Exception error) when (error is InvalidDataException or NotSupportedException)
        { throw new ModException("unreadable ZIP archive: " + error.GetType().Name, ModErrors.Archive); }
    }

    /// <summary>The game's <c>&lt;Azimuth&gt;\mods\&lt;game folder&gt;</c> (OD-233), created when missing; a linked one is refused.</summary>
    private string ImportRoot(string game)
    {
        var root = Safe(_azimuth, ExtraOptionsMods.ModsFolder + "/" + ExtraOptionsLayout.GameFolder(game));
        Directory.CreateDirectory(root);
        return root;
    }

    private ExtraOptionsModImport Import(string archivePath, string game)
    {
        CheckGame();
        var root = ImportRoot(game);
        using var zip = ZipFile.OpenRead(archivePath);
        if (zip.Entries.Count > MaxImportEntries) throw new ModException("too many archive entries", ModErrors.Archive);
        // Keyed without letter case: two entries one Windows folder would merge are refused, never resolved by order.
        var entries = new Dictionary<string, ZipArchiveEntry>(StringComparer.OrdinalIgnoreCase);
        foreach (var entry in zip.Entries)
        {
            var name = entry.FullName.Replace('\\', '/');
            if (name.EndsWith('/') || name.StartsWith("__MACOSX/", StringComparison.OrdinalIgnoreCase)) continue;
            if (!entries.TryAdd(name, entry)) throw new ModException("duplicate archive entries", ModErrors.Archive);
        }
        // The mod's mod.json: at the top, else in exactly one top folder (a top one wins: a folder's own mod.json is its content).
        var manifest = entries.ContainsKey(ModJson) ? ModJson : null;
        if (manifest is null)
        {
            var nested = entries.Keys.Where(k => k.Split('/') is [_, var file] && file.Equals(ModJson, StringComparison.OrdinalIgnoreCase)).ToArray();
            if (nested.Length > 1) throw new ModException("several mods in the archive", ModErrors.ArchiveManifest);
            manifest = nested.Length == 1 ? nested[0] : throw new ModException("no mod.json in the archive", ModErrors.ArchiveNoManifest);
        }
        var prefix = manifest[..^ModJson.Length];
        var manifestEntry = entries[manifest];
        if (manifestEntry.Length > MaxImportManifestBytes) throw new ModException("mod.json too large", ModErrors.ArchiveManifest);
        var (id, title, version) = ReadModJson(ReadEntry(manifestEntry, manifestEntry.Length));

        // Exactly the files of the mod's folder, each a plain file at a relative path below it.
        var files = entries.Where(e => e.Key.StartsWith(prefix, StringComparison.OrdinalIgnoreCase)).Select(e => (Path: e.Key[prefix.Length..], Entry: e.Value)).ToList();
        foreach (var (path, entry) in files)
        {
            // Unix mode in the high 16 bits (S_IFLNK), or a Windows reparse point in the low ones.
            if ((entry.ExternalAttributes >> 16 & 0xF000) == 0xA000 || (entry.ExternalAttributes & (int)FileAttributes.ReparsePoint) != 0)
                throw new ModException("archive holds a link: " + path, ModErrors.Archive);
            try { ModManifest.ValidatePath(path); }
            catch (ModException) { throw new ModException("archive path outside the mod folder: " + path, ModErrors.Archive); }
        }
        if (files.Sum(f => f.Entry.Length) > MaxImportExtractedBytes) throw new ModException("mod too large", ModErrors.Archive);
        // A file and a folder of the same name (`a` and `a/b`) cannot both be written: the archive is refused, never half extracted.
        var names = files.Select(f => f.Path).ToHashSet(StringComparer.OrdinalIgnoreCase);
        foreach (var (path, _) in files)
            for (var slash = path.IndexOf('/', StringComparison.Ordinal); slash > 0; slash = path.IndexOf('/', slash + 1))
                if (names.Contains(path[..slash])) throw new ModException("archive holds a file and a folder of the same name: " + path[..slash], ModErrors.Archive);

        using var gate = Lock();
        if (ExtraOptionsLayout.NeedsMigration(_azimuth)) throw new ModException("the mods folder must move to the per-game layout first", ModErrors.Layout);
        // A catalog mod's folder is the launcher's (its receipt restores it): never replaced behind it. A receipt that cannot be read
        // still claims its folder (OD-232: the folder is the id).
        var claimed = InstalledSnapshots(out var unreadable).Where(m => m.ModsGame == game).Select(ExtraOptionsMods.FolderOf).OfType<string>().Concat(unreadable);
        if (claimed.Any(f => string.Equals(f, id, StringComparison.OrdinalIgnoreCase)))
            throw new ModException("a mod the launcher installed has this id", ModErrors.Conflict);
        var target = Safe(root, id);
        var staging = Path.Combine(root, "." + Guid.NewGuid().ToString("N") + ImportSuffix);
        Directory.CreateDirectory(staging);
        try
        {
            long written = 0;
            foreach (var (path, entry) in files)
            {
                var destination = Resolve(staging, path);
                Directory.CreateDirectory(Path.GetDirectoryName(destination)!);
                using var input = entry.Open();
                using var output = new FileStream(destination, FileMode.CreateNew, FileAccess.Write, FileShare.None);
                var buffer = new byte[81920]; long total = 0;
                for (int read; (read = input.Read(buffer)) > 0;)
                {
                    total += read; written += read;
                    if (total > entry.Length || written > MaxImportExtractedBytes) throw new ModException("archive entry larger than declared: " + path, ModErrors.Archive);
                    output.Write(buffer, 0, read);
                }
                if (total != entry.Length) throw new ModException("archive entry shorter than declared: " + path, ModErrors.Archive);
            }
            CheckGame();
            string? aside = null, asidePath = null; var asideFolder = false;
            if (Directory.Exists(target) || File.Exists(target))
            {
                // The earlier folder (possibly the player's own edits) is kept aside, never deleted.
                var replaced = Safe(root, ReplacedFolder);
                Directory.CreateDirectory(replaced);
                aside = id + "." + DateTimeOffset.UtcNow.ToString("yyyyMMddHHmmss", System.Globalization.CultureInfo.InvariantCulture) + "." + Guid.NewGuid().ToString("N")[..8];
                asidePath = Path.Combine(replaced, aside); asideFolder = Directory.Exists(target);
                if (asideFolder) Directory.Move(target, asidePath);
                else File.Move(target, asidePath);
            }
            try
            {
                FaultPoint?.Invoke("import-move");
                Directory.Move(staging, target);
            }
            catch (Exception error) when (error is IOException or UnauthorizedAccessException && asidePath is not null)
            {
                // The new folder did not take its place: the earlier one goes back where it was (left in .replaced if even that fails).
                BestEffort(() =>
                {
                    if (Directory.Exists(target) || File.Exists(target)) return;
                    if (asideFolder) Directory.Move(asidePath!, target); else File.Move(asidePath!, target);
                });
                throw;
            }
            return new(id, title, version, aside);
        }
        finally { BestEffort(() => { if (Directory.Exists(staging)) RemoveStaging(staging); }); }
    }

    /// <summary>The loader's manifest rule (format_version 1, a valid id, <see cref="ExtraOptionsMods.ValidId"/>); the name and version are shown when present.</summary>
    private static (string Id, string Name, string? Version) ReadModJson(byte[] bytes)
    {
        try
        {
            using var document = JsonDocument.Parse(bytes, new JsonDocumentOptions { MaxDepth = 64 });
            var root = document.RootElement;
            if (root.ValueKind == JsonValueKind.Object && root.TryGetProperty("format_version", out var format) && format.ValueKind == JsonValueKind.Number
                && format.GetDouble() == 1 && root.TryGetProperty("id", out var idValue) && idValue.ValueKind == JsonValueKind.String
                && idValue.GetString() is { } id && ExtraOptionsMods.ValidId(id))
            {
                // the folder is named by the id: a name Windows reserves (con, nul, ...) is no folder
                ModManifest.ValidatePath(id);
                var name = root.TryGetProperty("name", out var n) && n.ValueKind == JsonValueKind.String && n.GetString() is { Length: > 0 } text ? text : id;
                var version = root.TryGetProperty("version", out var v) && v.ValueKind == JsonValueKind.String ? v.GetString() : null;
                return (id, name, version);
            }
        }
        catch (Exception error) when (error is JsonException or ModException) { }
        throw new ModException("mod.json is not a format 1 manifest with a valid id", ModErrors.ArchiveManifest);
    }

    private static byte[] ReadEntry(ZipArchiveEntry entry, long length)
    {
        using var stream = entry.Open();
        var bytes = new byte[checked((int)length)];
        stream.ReadExactly(bytes);
        if (stream.ReadByte() != -1) throw new ModException("archive entry larger than declared", ModErrors.Archive);
        return bytes;
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

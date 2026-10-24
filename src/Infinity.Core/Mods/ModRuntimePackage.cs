// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.IO.Compression;
using System.Text.Json;
using Infinity.Core.Process;

namespace Infinity.Core.Mods;

/// <summary>
/// The native mod runtime: <c>infinity_mod.dll</c> and <c>infinity-mod-prepare.exe</c>, each with its detached <c>.sig</c> in the
/// mod runtime's own domain (<see cref="ReleaseSignature.ModRuntimePrefix"/>), verified with the build's key (OD-49: the release
/// key in production; the development key against a development server). The hook re-checks the same signature and the hash the
/// launcher hands it.
/// <para>
/// OD-116: from 1.0.10 the launcher no longer bundles it. The catalog lists it as the entry <c>infinity-mod-runtime</c> (a ZIP of
/// the four files, pinned by SHA-256), installed under the data home, <c>&lt;home&gt;/mods/runtime/&lt;16 hex&gt;/</c>, with
/// <c>current.json</c> naming the one in use. Native mods need it; without a verified one they are listed as needing it.
/// </para>
/// </summary>
public sealed record ModRuntimePackage(string Runtime, string Helper)
{
    public const string Folder = "mod";
    public const string RuntimeFile = "infinity_mod.dll";
    public const string HelperFile = "infinity-mod-prepare.exe";
    private const string CurrentFile = "current.json";
    private const long MaxExpandedBytes = 128L * 1024 * 1024;
    private static readonly string[] PackageFiles = [RuntimeFile, RuntimeFile + ".sig", HelperFile, HelperFile + ".sig"];

    /// <summary>The installed package's catalog version and download SHA-256 (null for a bundled development package).</summary>
    public string? Version { get; init; }
    public string? PackageSha256 { get; init; }

    /// <summary>
    /// A bundled package under <paramref name="launcherDirectory"/> (<c>mod\</c>), or null. Development builds only since OD-116:
    /// production locates the installed catalog runtime (<see cref="LocateInstalled"/>).
    /// </summary>
    public static ModRuntimePackage? Locate(string launcherDirectory, ReadOnlySpan<byte> key)
    {
        ArgumentNullException.ThrowIfNull(launcherDirectory);
        return Verify(Path.Combine(launcherDirectory, Folder), key);
    }

    /// <summary>Both files present and signed with <paramref name="key"/> in <paramref name="folder"/>, or null.</summary>
    private static ModRuntimePackage? Verify(string folder, ReadOnlySpan<byte> key)
    {
        if (key.Length != 32) return null;
        var runtime = Path.Combine(folder, RuntimeFile);
        var helper = Path.Combine(folder, HelperFile);
        foreach (var path in new[] { runtime, helper })
            if (!File.Exists(path) || !File.Exists(path + ".sig") || !ReleaseSignature.VerifyFile(path, path + ".sig", key, ReleaseSignature.ModRuntimePrefix)) return null;
        return new(runtime, helper);
    }

    /// <summary>The installed runtimes' folder: <c>&lt;home&gt;/mods/runtime</c>.</summary>
    public static string InstalledRoot(string home) => Path.Combine(Path.GetFullPath(home), "mods", "runtime");

    private sealed record Current(string Version, string Sha256, string Folder);

    /// <summary>The runtime installed from a catalog (OD-116), verified with <paramref name="key"/>; null when none or it no longer verifies.</summary>
    public static ModRuntimePackage? LocateInstalled(string home, ReadOnlySpan<byte> key)
    {
        try
        {
            var root = InstalledRoot(home);
            var path = Path.Combine(root, CurrentFile);
            if (!File.Exists(path) || new FileInfo(path).Length > 4096) return null;
            var current = JsonSerializer.Deserialize<Current>(File.ReadAllBytes(path));
            if (current is null || !PlainFolder(current.Folder) || current.Sha256 is not { Length: 64 } || current.Version is null) return null;
            var folder = Path.Combine(root, current.Folder);
            if (IsLink(root) || IsLink(folder)) return null;
            return Verify(folder, key) is { } package ? package with { Version = current.Version, PackageSha256 = current.Sha256 } : null;
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException or JsonException) { return null; }
    }

    /// <summary>
    /// The update from 1.0.9: its launcher folder still holds the bundled, release-signed <c>mod\</c> (the update leaves it). When no
    /// catalog runtime is installed yet and that one verifies with <paramref name="key"/>, it is copied in as the installed runtime, so
    /// enabled native mods keep playing before the catalog is reachable. The catalog's runtime replaces it at the next native action.
    /// </summary>
    public static ModRuntimePackage? AdoptBundled(string launcherDirectory, string home, ReadOnlySpan<byte> key)
    {
        if (LocateInstalled(home, key) is { } installed) return installed;
        var bundled = Path.Combine(launcherDirectory, Folder);
        if (Verify(bundled, key) is null) return null;
        try
        {
            var root = InstalledRoot(home);
            Directory.CreateDirectory(root);
            if (IsLink(root)) return null;
            var digest = Convert.ToHexStringLower(System.Security.Cryptography.SHA256.HashData(File.ReadAllBytes(Path.Combine(bundled, RuntimeFile))));
            var name = digest[..16]; var folder = Path.Combine(root, name);
            if (Verify(folder, key) is null)
            {
                var staging = Path.Combine(root, "." + Guid.NewGuid().ToString("N") + ".staging");
                try
                {
                    Directory.CreateDirectory(staging);
                    foreach (var file in PackageFiles) File.Copy(Path.Combine(bundled, file), Path.Combine(staging, file));
                    if (Verify(staging, key) is null) return null;
                    if (Directory.Exists(folder)) Directory.Move(folder, Path.Combine(root, "." + Guid.NewGuid().ToString("N") + ".damaged"));
                    Directory.Move(staging, folder);
                }
                finally { if (Directory.Exists(staging)) Remove(staging); }
            }
            var current = Path.Combine(root, CurrentFile);
            File.WriteAllBytes(current + ".tmp", JsonSerializer.SerializeToUtf8Bytes(new Current("bundled", digest, name)));
            File.Move(current + ".tmp", current, overwrite: true);
            return LocateInstalled(home, key.ToArray());
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException) { return null; }
    }

    /// <summary>Whether the catalog's runtime differs from the installed one (none, another download, or not verifying).</summary>
    public static bool NeedsInstall(ModRuntimePackage? installed, ModRuntimeEntry? entry) =>
        entry is not null && installed?.PackageSha256 != entry.Download.Sha256;

    /// <summary>
    /// Downloads the catalog's runtime package, checks its size and SHA-256, extracts exactly the four runtime files (and its
    /// <c>licenses/</c> notices), verifies both signatures with <paramref name="key"/> and only then makes it current. A runtime
    /// that does not verify is refused and nothing changes; an earlier one stays in use. Earlier folders are pruned when not in use.
    /// </summary>
    public static async Task<ModRuntimePackage> InstallAsync(HttpClient http, string home, ModRuntimeEntry entry, byte[] key, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(http); ArgumentNullException.ThrowIfNull(entry); ArgumentNullException.ThrowIfNull(key);
        byte[] bytes;
        using (var response = await ModDownloads.DownloadAsync(http, entry.Download.Url, _ => true, cancellation).ConfigureAwait(false))
        {
            response.EnsureSuccessStatusCode();
            await using var body = await response.Content.ReadAsStreamAsync(cancellation).ConfigureAwait(false);
            bytes = await DevelopmentModCatalog.ReadBoundedAsync(body, entry.Download.Bytes, cancellation).ConfigureAwait(false);
        }
        if (bytes.LongLength != entry.Download.Bytes || ModManifest.Hash(bytes) != entry.Download.Sha256)
            throw new ModException("runtime package digest mismatch", ModErrors.Digest);
        return Install(home, entry, bytes, key);
    }

    /// <summary>The verified bytes of <paramref name="entry"/>'s package (see <see cref="InstallAsync"/>).</summary>
    internal static ModRuntimePackage Install(string home, ModRuntimeEntry entry, byte[] bytes, byte[] key)
    {
        if (bytes.LongLength != entry.Download.Bytes || ModManifest.Hash(bytes) != entry.Download.Sha256)
            throw new ModException("runtime package digest mismatch", ModErrors.Digest);
        var root = InstalledRoot(home);
        Directory.CreateDirectory(root);
        if (IsLink(root)) throw new ModException("linked runtime folder", ModErrors.Linked);
        var name = entry.Download.Sha256[..16];
        var folder = Path.Combine(root, name);
        if (Verify(folder, key) is null)
        {
            var staging = Path.Combine(root, "." + Guid.NewGuid().ToString("N") + ".staging");
            try
            {
                Directory.CreateDirectory(staging);
                Extract(bytes, staging);
                if (Verify(staging, key) is null) throw new ModException("the runtime's signature does not verify", ModErrors.Digest);
                if (Directory.Exists(folder)) Directory.Move(folder, Path.Combine(root, "." + Guid.NewGuid().ToString("N") + ".damaged"));
                Directory.Move(staging, folder);
            }
            finally { if (Directory.Exists(staging)) Remove(staging); }
        }
        var current = Path.Combine(root, CurrentFile);
        File.WriteAllBytes(current + ".tmp", JsonSerializer.SerializeToUtf8Bytes(new Current(entry.Version, entry.Download.Sha256, name)));
        File.Move(current + ".tmp", current, overwrite: true);
        Prune(root, name);
        return LocateInstalled(home, key) ?? throw new ModException("the installed runtime does not verify", ModErrors.Digest);
    }

    private static void Extract(byte[] bytes, string staging)
    {
        ZipArchive zip;
        try { zip = new ZipArchive(new MemoryStream(bytes), ZipArchiveMode.Read); }
        catch (InvalidDataException) { throw new ModException("the runtime package is not a ZIP", ModErrors.Digest); }
        using (zip)
        {
            var seen = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
            long expanded = 0; var licenses = 0;
            foreach (var entry in zip.Entries)
            {
                var name = entry.FullName.Replace('\\', '/');
                if (name.EndsWith('/')) continue;
                var license = name.StartsWith("licenses/", StringComparison.Ordinal) && name.Count(c => c == '/') == 1
                    && (name.EndsWith(".txt", StringComparison.OrdinalIgnoreCase) || name.EndsWith(".md", StringComparison.OrdinalIgnoreCase));
                // Bounded before anything is written: the two binaries, their signatures and a handful of notices.
                if (!PackageFiles.Contains(name, StringComparer.Ordinal) && !license || !seen.Add(name) || entry.Length > 64L * 1024 * 1024
                    || license && ++licenses > 16 || (expanded += entry.Length) > MaxExpandedBytes)
                    throw new ModException("unexpected runtime package entry", ModErrors.Digest);
                ModManifest.ValidatePath(name);
                var target = Path.Combine(staging, name.Replace('/', Path.DirectorySeparatorChar));
                Directory.CreateDirectory(Path.GetDirectoryName(target)!);
                try
                {
                    using var input = entry.Open();
                    using var output = new FileStream(target, FileMode.CreateNew, FileAccess.Write, FileShare.None);
                    var buffer = new byte[81920]; long total = 0;
                    for (int read; (read = input.Read(buffer)) > 0;)
                    {
                        if ((total += read) > entry.Length) throw new ModException("runtime package entry larger than declared", ModErrors.Digest);
                        output.Write(buffer, 0, read);
                    }
                }
                catch (Exception error) when (error is InvalidDataException or NotSupportedException) { throw new ModException("unreadable runtime package entry", ModErrors.Digest); }
            }
            if (PackageFiles.Any(f => !seen.Contains(f))) throw new ModException("the runtime package is incomplete", ModErrors.Digest);
        }
    }

    /// <summary>Earlier runtimes and leftovers, best effort: a folder a running game still holds open stays until the next install.</summary>
    private static void Prune(string root, string keep)
    {
        foreach (var folder in Directory.EnumerateDirectories(root))
        {
            var name = Path.GetFileName(folder);
            if (name == keep || IsLink(folder) || !(PlainFolder(name) || name.StartsWith('.'))) continue;
            try { Remove(folder); } catch (Exception error) when (error is IOException or UnauthorizedAccessException) { }
        }
    }

    /// <summary>Removes a folder of plain runtime files (only ever created by <see cref="Install"/>); links are never followed.</summary>
    private static void Remove(string folder)
    {
        if (IsLink(folder)) return;
        foreach (var child in Directory.EnumerateDirectories(folder)) Remove(child);
        foreach (var file in Directory.EnumerateFiles(folder)) ManagedModFiles.RemoveFile(file);
        Directory.Delete(folder);
    }

    private static bool PlainFolder(string? name) => name is { Length: 16 } && name.All(c => char.IsAsciiDigit(c) || c is >= 'a' and <= 'f');

    /// <summary>
    /// A link or junction (never followed or removed through). Under Wine every mount point reports the reparse attribute, so there
    /// only a resolvable link target counts. The hook itself checks only the runtime's own folder and file for links (mod_host.c).
    /// </summary>
    private static bool IsLink(string path) => Directory.Exists(path)
        && (new DirectoryInfo(path).LinkTarget is not null || !WineHost.Detected && (File.GetAttributes(path) & FileAttributes.ReparsePoint) != 0);
}

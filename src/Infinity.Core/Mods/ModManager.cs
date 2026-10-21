// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.IO.Compression;
using System.Net;
using System.Net.Http.Json;
using System.Security.Cryptography;
using System.Text.Json;

namespace Infinity.Core.Mods;

/// <summary>A mod the launcher refused to change, or a download that did not verify.</summary>
public sealed class ModException(string message) : Exception(message);

/// <summary>
/// What the Azimuth folder holds of one mod. <see cref="Official"/>: every binary's MD5 equals a
/// vetted release's (<see cref="Version"/>); a binary present with another MD5 is
/// <see cref="Unrecognized"/> (modified, unofficial, or another mod with the same file name).
/// </summary>
public sealed record ModState(ModInfo Mod, bool Installed, bool Enabled, ModRelease? Version, bool Unrecognized, string? BinaryMd5, bool ConfigPresent)
{
    public bool Official => Version is not null && !Unrecognized;
}

/// <summary>The newest release on the official repository, and whether the catalog vetted it.</summary>
public sealed record ModUpdate(ModVersion Version, string AssetName, Uri DownloadUrl, string? Digest, ModRelease? Vetted)
{
    /// <summary>Installable: vetted, and GitHub's digest (when it gives one) is the catalog's SHA-256.</summary>
    public bool Verified => Vetted is not null && (Digest is null || Digest == "sha256:" + Vetted.AssetSha256);
}

/// <summary>
/// The Mods screen's actions on an Azimuth folder (OD-45, docs/launcher/MODS.md). Every change
/// needs the game closed (<paramref name="gameRunning"/>) and goes through renames and backups
/// under <c>data\fi-launcher\mods\&lt;id&gt;\</c>: nothing is deleted.
/// </summary>
public sealed class ModManager(string azimuthRoot, Func<bool> gameRunning)
{
    private const long MaxAssetBytes = 64L * 1024 * 1024;

    public string Root { get; } = azimuthRoot ?? throw new ArgumentNullException(nameof(azimuthRoot));

    public string ModDir(ModInfo mod) => Path.Combine(Root, "data", "fi-launcher", "mods", mod.Id);

    private string Disabled(ModInfo mod, ModFile file) => Path.Combine(ModDir(mod), "disabled", file.Destination);

    private string Live(ModFile file) => Path.Combine(Root, file.Destination);

    /// <summary>Reads the folder: where each binary is (live or set aside) and whether its MD5 is a vetted release's.</summary>
    public ModState State(ModInfo mod)
    {
        ArgumentNullException.ThrowIfNull(mod);
        var binary = mod.Latest.Binaries.First();
        var live = Live(binary);
        var disabled = Disabled(mod, binary);
        var enabled = File.Exists(live);
        var present = enabled ? live : File.Exists(disabled) ? disabled : null;
        var config = mod.Latest.Files.FirstOrDefault(f => f.Role == ModFileRole.Config);
        var configPresent = config is not null && File.Exists(Live(config));
        if (present is null)
        {
            return new ModState(mod, false, false, null, false, null, configPresent);
        }
        var md5 = Md5Of(present);
        var release = mod.Releases.FirstOrDefault(r => r.Binaries.Any(b => b.Destination == binary.Destination && b.Md5 == md5));
        return new ModState(mod, true, enabled, release, release is null, md5, configPresent);
    }

    /// <summary>On: the set-aside binaries go back next to azimuth.exe. Off: they move aside (a rename; the game no longer loads them).</summary>
    public void SetEnabled(ModInfo mod, bool enabled)
    {
        ArgumentNullException.ThrowIfNull(mod);
        RefuseWhileRunning();
        foreach (var binary in mod.Latest.Binaries)
        {
            var (live, aside) = (Live(binary), Disabled(mod, binary));
            if (enabled && !File.Exists(live) && File.Exists(aside))
            {
                Move(aside, live);
            }
            else if (!enabled && File.Exists(live))
            {
                if (File.Exists(aside))
                {
                    // never overwrite a set-aside copy without keeping it (another winmm.dll may have been dropped in)
                    var backup = NewBackupDir(mod, DateTime.UtcNow);
                    File.Copy(aside, Path.Combine(backup, binary.Destination + ".off"));
                }
                Move(live, aside);
            }
        }
    }

    /// <summary>
    /// The newest release of the official repository (GitHub's API, <c>/releases</c>): the highest
    /// version among assets named like the catalog's. Null when the repository lists none.
    /// </summary>
    public static async Task<ModUpdate?> CheckAsync(HttpClient http, ModInfo mod, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(http);
        ArgumentNullException.ThrowIfNull(mod);
        using var request = new HttpRequestMessage(HttpMethod.Get, $"https://api.github.com/repos/{mod.GitHubRepo}/releases?per_page=20");
        request.Headers.TryAddWithoutValidation("User-Agent", "infinity-launcher");
        request.Headers.TryAddWithoutValidation("Accept", "application/vnd.github+json");
        using var response = await http.SendAsync(request, cancellation).ConfigureAwait(false);
        response.EnsureSuccessStatusCode();
        var releases = await response.Content.ReadFromJsonAsync<JsonElement>(cancellation).ConfigureAwait(false);
        var prefix = mod.Latest.AssetName[..(mod.Latest.AssetName.IndexOf(mod.Latest.Version, StringComparison.Ordinal))];
        ModUpdate? best = null;
        foreach (var release in releases.EnumerateArray())
        {
            if ((release.TryGetProperty("draft", out var draft) && draft.ValueKind == JsonValueKind.True)
                || (release.TryGetProperty("prerelease", out var pre) && pre.ValueKind == JsonValueKind.True))
            {
                continue;
            }
            foreach (var asset in release.GetProperty("assets").EnumerateArray())
            {
                var name = asset.GetProperty("name").GetString() ?? string.Empty;
                if (!name.StartsWith(prefix, StringComparison.Ordinal) || ModVersion.TryParse(name[prefix.Length..]) is not { } version)
                {
                    continue;
                }
                if (!Uri.TryCreate(asset.TryGetProperty("browser_download_url", out var u) ? u.GetString() : null, UriKind.Absolute, out var url)
                    || url.Scheme != Uri.UriSchemeHttps || url.Host != "github.com")
                {
                    continue;
                }
                var digest = asset.TryGetProperty("digest", out var d) && d.ValueKind == JsonValueKind.String ? d.GetString() : null;
                // the same version twice (a debug build next to the release): the vetted asset wins
                var vetted = mod.Releases.FirstOrDefault(r => r.AssetName == name);
                if (best is null || version > best.Version || (version.CompareTo(best.Version) == 0 && best.Vetted is null && vetted is not null))
                {
                    best = new ModUpdate(version, name, url, digest, vetted);
                }
            }
        }
        return best;
    }

    /// <summary>Whether <paramref name="update"/> is newer than what is installed (or nothing is).</summary>
    public static bool IsNewer(ModState state, ModUpdate? update)
    {
        ArgumentNullException.ThrowIfNull(state);
        return update is not null && (state.Version is null || update.Version > ModVersion.Parse(state.Version.Version));
    }

    /// <summary>
    /// Installs or updates to a vetted release: the official asset is downloaded, its SHA-256 must
    /// be the catalog's, and only the catalog's files are taken from it, each by MD5. Binaries
    /// replace the current ones after a backup (in the "off" place when the mod is off); the
    /// player's INI and Gamerpic are installed only when absent. Returns the backup folder.
    /// </summary>
    public async Task<string> InstallAsync(HttpClient http, ModInfo mod, ModRelease release, Uri download, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(http);
        ArgumentNullException.ThrowIfNull(mod);
        ArgumentNullException.ThrowIfNull(release);
        ArgumentNullException.ThrowIfNull(download);
        if (!mod.Releases.Contains(release))
        {
            throw new ModException($"{mod.Name} {release.Version} is not a vetted release");
        }
        if (download != release.DownloadUrl(mod))
        {
            throw new ModException("downloads come only from github.com");
        }
        RefuseWhileRunning();
        using var response = await DownloadAsync(http, download, cancellation).ConfigureAwait(false);
        response.EnsureSuccessStatusCode();
        if (response.Content.Headers.ContentLength > MaxAssetBytes)
        {
            throw new ModException("the download is larger than any release");
        }
        using var buffer = new MemoryStream();
        await using (var body = await response.Content.ReadAsStreamAsync(cancellation).ConfigureAwait(false))
        {
            var chunk = new byte[81920];
            int read;
            while ((read = await body.ReadAsync(chunk, cancellation).ConfigureAwait(false)) > 0)
            {
                if (buffer.Length + read > MaxAssetBytes)
                {
                    throw new ModException("the download is larger than any release");
                }
                buffer.Write(chunk, 0, read);
            }
        }
        return Install(mod, release, buffer.ToArray(), DateTime.UtcNow);
    }

    // The supplied client must disable automatic redirects and carry no credentials/cookies.
    // GitHub serves release bytes through its CDN; validate each hop before sending it.
    private static async Task<HttpResponseMessage> DownloadAsync(HttpClient http, Uri location, CancellationToken cancellation)
    {
        if (http.DefaultRequestHeaders.Any())
            throw new ModException("a dedicated download client is required");
        var visited = new HashSet<Uri>();
        for (var redirects = 0; ; redirects++)
        {
            if (!location.IsAbsoluteUri || location.Scheme != Uri.UriSchemeHttps || !location.IsDefaultPort
                || location.UserInfo.Length != 0 || location.Fragment.Length != 0
                || location.Host is not ("github.com" or "release-assets.githubusercontent.com" or "objects.githubusercontent.com" or "github-releases.githubusercontent.com")
                || !visited.Add(location))
                throw new ModException("the mod download destination is not supported");
            using var request = new HttpRequestMessage(HttpMethod.Get, location);
            request.Headers.TryAddWithoutValidation("User-Agent", "infinity-launcher");
            var response = await http.SendAsync(request, HttpCompletionOption.ResponseHeadersRead, cancellation).ConfigureAwait(false);
            if (response.RequestMessage?.RequestUri is { } actual && actual != location)
            {
                response.Dispose();
                throw new ModException("automatic download redirects are not supported");
            }
            if (response.StatusCode is not (HttpStatusCode.MovedPermanently or HttpStatusCode.Found or HttpStatusCode.SeeOther
                or HttpStatusCode.TemporaryRedirect or HttpStatusCode.PermanentRedirect))
                return response;
            var next = response.Headers.Location;
            response.Dispose();
            if (redirects >= 5 || next is null || !Uri.TryCreate(location, next, out var resolved))
                throw new ModException("the mod download redirected too many times or has no destination");
            location = resolved;
        }
    }

    /// <summary><see cref="InstallAsync"/> from the asset's bytes (tests, and a download already in hand).</summary>
    public string Install(ModInfo mod, ModRelease release, byte[] asset, DateTime nowUtc)
    {
        ArgumentNullException.ThrowIfNull(mod);
        ArgumentNullException.ThrowIfNull(release);
        ArgumentNullException.ThrowIfNull(asset);
        RefuseWhileRunning();
        if (Convert.ToHexStringLower(SHA256.HashData(asset)) != release.AssetSha256)
        {
            throw new ModException($"the download is not the official {release.AssetName} (SHA-256)");
        }
        var files = new Dictionary<ModFile, byte[]>();
        using (var zip = new ZipArchive(new MemoryStream(asset), ZipArchiveMode.Read))
        {
            foreach (var file in release.Files)
            {
                var entry = zip.Entries.FirstOrDefault(e => e.FullName.Replace('\\', '/') == file.ZipPath)
                    ?? throw new ModException($"{file.ZipPath} is missing from the release");
                using var stream = entry.Open();
                using var copy = new MemoryStream();
                stream.CopyTo(copy);
                var bytes = copy.ToArray();
                if (Md5Hex(bytes) != file.Md5)
                {
                    throw new ModException($"{file.ZipPath} is not the official file (MD5)");
                }
                files[file] = bytes;
            }
        }
        var enabled = State(mod) is { Installed: false } or { Enabled: true };
        var backup = Path.Combine(ModDir(mod), "backup", Stamp(nowUtc));
        var backupUsed = false;
        foreach (var (file, bytes) in files)
        {
            var target = file.Role == ModFileRole.Binary && !enabled ? Disabled(mod, file) : Live(file);
            if (file.Role != ModFileRole.Binary && File.Exists(target))
            {
                continue; // the player's INI and picture stay theirs
            }
            foreach (var existing in new[] { Live(file), Disabled(mod, file) }.Where(File.Exists))
            {
                if (!backupUsed)
                {
                    backup = NewBackupDir(mod, nowUtc);
                    backupUsed = true;
                }
                File.Copy(existing, Path.Combine(backup, Path.GetFileName(existing) + (existing == Live(file) ? string.Empty : ".off")), overwrite: true);
            }
            var temp = target + ".fi-new";
            Directory.CreateDirectory(Path.GetDirectoryName(target)!);
            using (var stream = new FileStream(temp, FileMode.Create, FileAccess.Write, FileShare.None))
            {
                stream.Write(bytes);
                stream.Flush(flushToDisk: true);
            }
            File.Move(temp, target, overwrite: true);
            if (file.Role == ModFileRole.Binary)
            {
                // one copy only: the live one when on, the set-aside one when off
                var other = target == Live(file) ? Disabled(mod, file) : Live(file);
                if (File.Exists(other))
                {
                    File.Delete(other); // backed up above
                }
            }
        }
        if (State(mod) is not { Official: true } after || after.Version != release)
        {
            throw new ModException($"{mod.Name} {release.Version} did not verify after installing");
        }
        if (mod.Id == ModCatalog.ExtraOptionsId)
        {
            EnforceInfinityLocks(mod, nowUtc); // the official INI turns the conflicting switches on
        }
        return backup;
    }

    /// <summary>The INI's path, and the backup path a save of it uses.</summary>
    public (string Path, string Backup) IniPaths(ModInfo mod, DateTime nowUtc) =>
        (Path.Combine(Root, ExtraOptionsIni.FileName), Path.Combine(NewBackupDir(mod, nowUtc), ExtraOptionsIni.FileName));

    private static string Stamp(DateTime nowUtc) => nowUtc.ToString("yyyyMMdd'T'HHmmssfff'Z'", CultureInfo.InvariantCulture);

    /// <summary>A new, empty backup folder: the stamp to the millisecond, then -1, -2… so no backup ever replaces another.</summary>
    public string NewBackupDir(ModInfo mod, DateTime nowUtc)
    {
        ArgumentNullException.ThrowIfNull(mod);
        var root = Path.Combine(ModDir(mod), "backup");
        var name = Stamp(nowUtc);
        var dir = Path.Combine(root, name);
        for (var i = 1; Directory.Exists(dir); i++)
        {
            dir = Path.Combine(root, $"{name}-{i}");
        }
        Directory.CreateDirectory(dir);
        return dir;
    }

    /// <summary>
    /// Keeps the switches that conflict with Infinity off (<see cref="IniOption.LockedOff"/>): when
    /// the INI has one on (or missing, the mod's default being on), it is set to 0 after a backup.
    /// Before every Infinity launch and after an install. Whether the file changed; refused while
    /// the game runs.
    /// </summary>
    public bool EnforceInfinityLocks(ModInfo mod, DateTime nowUtc)
    {
        ArgumentNullException.ThrowIfNull(mod);
        var path = Path.Combine(Root, ExtraOptionsIni.FileName);
        if (!File.Exists(path))
        {
            return false;
        }
        RefuseWhileRunning();
        var ini = ExtraOptionsIni.Load(path);
        if (!ini.EnforceInfinityLocks())
        {
            return false;
        }
        SaveIni(mod, ini, nowUtc);
        return true;
    }

    /// <summary>Saves the INI form (values only), after a backup; refused while the game runs.</summary>
    public void SaveIni(ModInfo mod, ExtraOptionsIni ini, DateTime nowUtc)
    {
        ArgumentNullException.ThrowIfNull(ini);
        RefuseWhileRunning();
        var (path, backup) = IniPaths(mod, nowUtc);
        ini.Save(path, backup);
    }

    private void RefuseWhileRunning()
    {
        if (gameRunning())
        {
            throw new ModException("close the game first");
        }
    }

    private static void Move(string from, string to)
    {
        Directory.CreateDirectory(Path.GetDirectoryName(to)!);
        File.Move(from, to, overwrite: true);
    }

    public static string Md5Of(string path)
    {
        using var stream = File.OpenRead(path);
#pragma warning disable CA5351 // The owner asked for MD5 to show the official build (OD-45); downloads are verified by SHA-256.
        return Convert.ToHexStringLower(MD5.HashData(stream));
#pragma warning restore CA5351
    }

    private static string Md5Hex(byte[] bytes)
    {
#pragma warning disable CA5351 // As above: MD5 identifies the official files; the asset itself is checked by SHA-256.
        return Convert.ToHexStringLower(MD5.HashData(bytes));
#pragma warning restore CA5351
    }
}

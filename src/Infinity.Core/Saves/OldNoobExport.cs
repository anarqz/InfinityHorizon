// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.IO.Compression;
using Infinity.Core.Api;

namespace Infinity.Core.Saves;

/// <summary>One old Noob cloud line of the account (OD-239 item 6): the AZIMUTH profile's career that still has a cloud save, read-only.</summary>
public sealed record OldNoobLine(string ProfileId, string Gamertag, string Xuid, string Game, string CareerId, long Revision, string SavedAt);

/// <summary>How an export ended.</summary>
public enum OldNoobExportOutcome
{
    /// <summary>The save is in the file the player chose (<see cref="OldNoobExportResult.File"/>).</summary>
    Written,

    /// <summary>The chosen folder is Azimuth's (its saves or the install): nothing was requested or written.</summary>
    Refused,

    /// <summary>The line holds no revision any more (erased, or never saved): nothing was written.</summary>
    NoRevision,

    /// <summary>The download, its checks or the write failed (<see cref="OldNoobExportResult.Code"/>): nothing was left behind.</summary>
    Failed,
}

/// <summary>What <see cref="OldNoobExport.ExportAsync"/> did.</summary>
public sealed record OldNoobExportResult(OldNoobExportOutcome Outcome, string? File = null, ulong? Revision = null, string? Code = null, string? Message = null);

/// <summary>
/// OD-239 items 6-7: an AZIMUTH profile has no cloud save any more, but the cloud keeps an old Noob line read-only until it is erased 90 days
/// after the Azimuth release. The launcher offers once to export its newest revision (SAVES_API.md §4.2: <c>revisions?limit=1</c>, then its
/// bundle, verified as every download is) to a file in a folder the player picks; it is never written into Azimuth's saves or install, and
/// nothing on this PC (the old <c>User_&lt;XUID&gt;</c> folder included) is changed. The file is a ZIP of the revision folder
/// (<c>revisions/&lt;label&gt;/…</c>), the layout Azimuth keeps on disk, so a player can copy progress by hand.
/// </summary>
public static class OldNoobExport
{
    /// <summary>The refusal's string key (en + pt-BR).</summary>
    public const string RefusedKey = "export.refused_azimuth_folder";

    /// <summary>
    /// The adapter (the one place that reads which old lines exist): today's <c>/me</c>, an active <c>noob</c> profile's career whose
    /// <c>cloud</c> still names a revision. MACBOOK keeps exposing that line, marked read-only, until the erase; when its contract names the
    /// read-only field, it is read here (and only here), the rest of the launcher unchanged.
    /// </summary>
    public static IReadOnlyList<OldNoobLine> Lines(Me? me)
    {
        if (me?.Players is not { } players) return [];
        return [.. players.Where(p => p.Mode == "noob" && p.Status == "active")
            .SelectMany(p => p.Careers.Where(c => c.Cloud is { Revision: > 0 })
                .Select(c => new OldNoobLine(p.ProfileId, p.Gamertag, p.Xuid, c.Game, c.CareerId, c.Cloud!.Revision, c.Cloud.SavedAt)))];
    }

    /// <summary>The file's name in the chosen folder: <c>Infinity-old-Noob-save-&lt;gamertag&gt;-&lt;game&gt;-r&lt;revision&gt;.zip</c>.</summary>
    public static string FileName(OldNoobLine line)
    {
        ArgumentNullException.ThrowIfNull(line);
        var invalid = Path.GetInvalidFileNameChars();
        var name = new string([.. Infinity.Core.Shell.ProfileName.Plain(line.Gamertag).Select(ch => invalid.Contains(ch) || ch == ' ' ? '_' : ch)]);
        var game = new string([.. line.Game.Where(char.IsAsciiLetterOrDigit)]);
        return string.Create(CultureInfo.InvariantCulture, $"Infinity-old-Noob-save-{name}-{game}-r{line.Revision}.zip");
    }

    /// <summary>
    /// Why <paramref name="folder"/> may not take the export, or null: it is (or is inside) one of <paramref name="forbidden"/> (Azimuth's
    /// install, which holds every game's saves), or it is not a folder that exists.
    /// </summary>
    public static string? Refusal(string? folder, IEnumerable<string> forbidden)
    {
        ArgumentNullException.ThrowIfNull(forbidden);
        if (Full(folder) is not { } target || !Directory.Exists(target)) return "export.folder_missing";
        foreach (var root in forbidden)
        {
            if (Full(root) is not { } r) continue;
            if (string.Equals(target, r, StringComparison.OrdinalIgnoreCase)
                || target.StartsWith(r + Path.DirectorySeparatorChar, StringComparison.OrdinalIgnoreCase)) return RefusedKey;
        }
        return null;
    }

    /// <summary>
    /// Downloads <paramref name="line"/>'s newest revision and writes it to a new file in <paramref name="folder"/> (an existing file is never
    /// replaced: <c>(2)</c>, <c>(3)</c>… are tried). The folder is checked before anything is requested and again before the write.
    /// </summary>
    public static async Task<OldNoobExportResult> ExportAsync(ISaveApi api, string access, OldNoobLine line, string folder, IReadOnlyList<string> forbidden,
        CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(api);
        ArgumentNullException.ThrowIfNull(line);
        ArgumentNullException.ThrowIfNull(forbidden);
        if (Refusal(folder, forbidden) is { } refused) return new(OldNoobExportOutcome.Refused, Code: refused);
        var source = await ProfileSeed.FetchSourceAsync(api, access, line.Xuid, cancellation).ConfigureAwait(false);
        if (source.IsNoRevision) return new(OldNoobExportOutcome.NoRevision);
        if (source.Packed is not { } packed)
            return new(OldNoobExportOutcome.Failed, Revision: source.Revision, Code: source.Error?.Code, Message: source.Error?.Message);
        if (Refusal(folder, forbidden) is { } late) return new(OldNoobExportOutcome.Refused, Code: late);
        var target = Full(folder)!;
        var label = packed.Number.ToString("x16", CultureInfo.InvariantCulture);
        string? temp = null;
        try
        {
            var zip = Zip(label, packed.Files);
            var path = FreePath(target, FileName(line));
            temp = path + ".part";
            await File.WriteAllBytesAsync(temp, zip, cancellation).ConfigureAwait(false);
            File.Move(temp, path, overwrite: false);
            temp = null;
            return new(OldNoobExportOutcome.Written, File: path, Revision: source.Revision);
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
            return new(OldNoobExportOutcome.Failed, Revision: source.Revision, Code: "LOCAL.WRITE_FAILED", Message: e.Message);
        }
        finally
        {
            if (temp is not null)
            {
                try { File.Delete(temp); }
                catch (Exception e) when (e is IOException or UnauthorizedAccessException) { }
            }
        }
    }

    /// <summary>The ZIP: every file of the revision under <c>revisions/&lt;label&gt;/</c>, as Azimuth lays it out.</summary>
    public static byte[] Zip(string label, IReadOnlyList<SaveFile> files)
    {
        ArgumentNullException.ThrowIfNull(files);
        using var buffer = new MemoryStream();
        using (var archive = new ZipArchive(buffer, ZipArchiveMode.Create, leaveOpen: true))
        {
            foreach (var file in files)
            {
                var entry = archive.CreateEntry("revisions/" + label + "/" + file.Name.Replace('\\', '/'), CompressionLevel.Optimal);
                using var stream = entry.Open();
                stream.Write(file.Bytes);
            }
        }
        return buffer.ToArray();
    }

    private static string FreePath(string folder, string name)
    {
        var path = Path.Combine(folder, name);
        var stem = Path.GetFileNameWithoutExtension(name);
        var ext = Path.GetExtension(name);
        for (var n = 2; File.Exists(path) || File.Exists(path + ".part"); n++)
            path = Path.Combine(folder, string.Create(CultureInfo.InvariantCulture, $"{stem} ({n}){ext}"));
        return path;
    }

    private static string? Full(string? path)
    {
        if (string.IsNullOrWhiteSpace(path)) return null;
        try { return Path.TrimEndingDirectorySeparator(Path.GetFullPath(path)); }
        catch (Exception e) when (e is ArgumentException or NotSupportedException or PathTooLongException) { return null; }
    }
}

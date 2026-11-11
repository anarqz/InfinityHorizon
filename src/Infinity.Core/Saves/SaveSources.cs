// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Security.Cryptography;
using System.Text;
using System.Text.RegularExpressions;
using Infinity.Core.Api;
using Infinity.Core.Process;
using Infinity.Core.Shell;

namespace Infinity.Core.Saves;

/// <summary>Who a save container on this PC belongs to, as the player knows it (never its <c>User_&lt;hex&gt;</c> name, which carries a XUID).</summary>
public enum SaveSourceKind
{
    /// <summary>A HARDCORE profile's own folder.</summary>
    Hardcore,

    /// <summary>An AZIMUTH (wire <c>noob</c>) profile's old folder, from before OD-239 sent Azimuth play to <c>User_9000000000000</c>.</summary>
    OldNoob,

    /// <summary>A folder no profile of this account or PC names.</summary>
    Unknown,

    /// <summary>A copy the launcher kept: its history (<see cref="LocalHistory"/>) or Azimuth's <c>fi-launcher-archive</c>.</summary>
    Backup,

    /// <summary>Only as <see cref="SaveSource.BackupOf"/>: a backup of the Azimuth save itself.</summary>
    Azimuth,
}

/// <summary>A profile that owns save folders on this PC: its kind (<see cref="SaveSourceKind.Hardcore"/> or <see cref="SaveSourceKind.OldNoob"/>) and plain name.</summary>
public sealed record SaveOwner(SaveSourceKind Kind, string Gamertag);

/// <summary>
/// One save the player can make their Azimuth save ("Use another save on this PC…", owner 2026-11-11): its newest sealed revision as read now
/// (<see cref="Save"/>: credits, XP, level, modified), who it belongs to, and the folder its sibling containers are kept in
/// (<see cref="SiblingsDir"/>: FM5's liveries and thumbnails; the game's own <c>player-1</c> for a folder or archive of this PC).
/// <see cref="Key"/> is opaque (a hash of the path): it never names a folder.
/// </summary>
public sealed record SaveSource(string Key, SaveSourceKind Kind, SaveProfile Save, string SiblingsDir)
{
    /// <summary>The owner's plain name, when a profile owns it.</summary>
    public string? Gamertag { get; init; }

    /// <summary>A backup's: when the launcher made it.</summary>
    public DateTime? BackupAtUtc { get; init; }

    /// <summary>A backup's: whose save it copied (<see cref="SaveSourceKind.Azimuth"/>, a profile's, or null when not known).</summary>
    public SaveSourceKind? BackupOf { get; init; }
}

/// <summary>
/// Owner 2026-11-11 (T-1111-AZIMUTH): every save container of one game on this PC that can become the Azimuth save: each
/// <c>player-1\User_*</c> folder but <c>User_9000000000000</c> itself (old Noob folders, HARDCORE folders, unknown ones), every container of
/// the launcher's history entries, and every revision in <c>fi-launcher-archive</c>. Only sealed revisions; one entry per content (the same
/// commit file twice is listed once, a profile folder before a backup), and never the content the Azimuth save already has. Reads only.
/// </summary>
public static partial class SaveSources
{
    /// <summary>
    /// Who owns which XUID (HARDCORE first: a XUID any HARDCORE profile has is never called an old Noob one): the account's players and
    /// profiles of the cached <paramref name="me"/>, this PC's local profiles, and the AZIMUTH profiles' old folders (<see cref="OldSaves.Owners"/>).
    /// </summary>
    public static IReadOnlyDictionary<ulong, SaveOwner> Owners(Me? me, IEnumerable<LocalProfile> locals)
    {
        ArgumentNullException.ThrowIfNull(locals);
        var all = locals.ToList();
        var owners = new Dictionary<ulong, SaveOwner>();
        void Add(string? xuid, SaveSourceKind kind, string name) { if (Parse(xuid) is { } x) owners.TryAdd(x, new SaveOwner(kind, name)); }
        foreach (var p in me?.Players ?? []) if (p.Mode != "noob") Add(p.Xuid, SaveSourceKind.Hardcore, ProfileName.Plain(p.Gamertag));
        foreach (var p in me?.Profiles ?? []) if (p.Mode != "noob") Add(p.Xuid, SaveSourceKind.Hardcore, ProfileName.Plain(p.Gamertag));
        foreach (var l in all.Where(l => l.Mode != "noob"))
        {
            owners.TryAdd(l.OfflineXuid, new SaveOwner(SaveSourceKind.Hardcore, l.DisplayName));
            Add(l.Cloud?.Xuid, SaveSourceKind.Hardcore, l.DisplayName);
        }
        foreach (var o in OldSaves.Owners(me, all)) owners.TryAdd(o.Xuid, new SaveOwner(SaveSourceKind.OldNoob, o.Gamertag));
        owners.Remove(AzimuthSaves.OfflineXuid);
        return owners;
    }

    /// <summary>The saves of <paramref name="game"/> under <paramref name="savesRoot"/> and in <paramref name="history"/>: profile folders first, then backups, newest first in each.</summary>
    public static IReadOnlyList<SaveSource> Scan(GameDefinition game, string savesRoot, LocalHistory history, IReadOnlyDictionary<ulong, SaveOwner> owners)
    {
        ArgumentNullException.ThrowIfNull(game);
        ArgumentNullException.ThrowIfNull(savesRoot);
        ArgumentNullException.ThrowIfNull(history);
        ArgumentNullException.ThrowIfNull(owners);
        var layout = game.Saves;
        var player = Path.Combine(savesRoot, layout.PlayerDir);
        var azimuthName = layout.UserFolder(AzimuthSaves.OfflineXuid);
        var seen = new HashSet<string>(StringComparer.Ordinal);
        if (Safe(() => AzimuthSaves.NewestSealed(Path.Combine(player, azimuthName))) is { } current && CommitHash(current) is { } now) seen.Add(now);
        var profiles = new List<SaveSource>();
        var backups = new List<SaveSource>();

        // 1. this PC's profile folders
        foreach (var dir in Folders(player, layout.UserPrefix))
        {
            var name = Path.GetFileName(dir);
            var xuid = XuidOf(name, layout);
            if (xuid == AzimuthSaves.OfflineXuid || Safe(() => AzimuthSaves.NewestSealed(dir)) is not { } revision || !New(revision)) continue;
            var owner = xuid is { } x && owners.TryGetValue(x, out var o) ? o : null;
            profiles.Add(new SaveSource(Key("pc", revision.Path), owner?.Kind ?? SaveSourceKind.Unknown, Profile(name, xuid, revision), player) { Gamertag = owner?.Gamertag });
        }

        // 2. the launcher's history: each entry's containers (a whole player-1, named containers, or one bare container)
        foreach (var item in Safe(history.List) ?? [])
        {
            var saves = Path.Combine(item.Folder, LocalHistory.SavesDir);
            var at = DateTimeOffset.TryParse(item.Entry.At, CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal, out var t) ? t.UtcDateTime : (DateTime?)null;
            var containers = Folders(saves, layout.UserPrefix).Select(d => (Dir: d, Name: (string?)Path.GetFileName(d), Siblings: saves)).ToList();
            if (Directory.Exists(Path.Combine(saves, layout.RevisionsDir))) containers.Add((saves, null, player));
            foreach (var (dir, name, siblings) in containers)
            {
                if (Safe(() => AzimuthSaves.NewestSealed(dir)) is not { } revision || !New(revision)) continue;
                var xuid = name is null ? Parse(item.Entry.Profile?.Xuid) : XuidOf(name, layout);
                backups.Add(new SaveSource(Key("history", revision.Path), SaveSourceKind.Backup, Profile("history-" + item.Name, xuid, revision), siblings)
                    { BackupAtUtc = at, BackupOf = Whose(xuid, owners, out var gamertag), Gamertag = gamertag });
            }
        }

        // 3. the copies a clone or install kept in fi-launcher-archive (one revision folder each)
        var archive = Path.Combine(savesRoot, AzimuthSaves.ArchiveDir);
        foreach (var container in Folders(archive, layout.UserPrefix))
        {
            var xuid = XuidOf(Path.GetFileName(container), layout);
            foreach (var dir in Folders(container, string.Empty))
            {
                var label = Path.GetFileName(dir);
                var number = RevisionInName().Match(label) is { Success: true } m ? ulong.Parse(m.Groups[1].Value, NumberStyles.AllowHexSpecifier, CultureInfo.InvariantCulture) : 0;
                if (Safe(() => AzimuthSaves.ReadRevision(number, dir)) is not { } revision || Safe(() => AzimuthSaves.Verify(revision)) is not { Count: 0 } || !New(revision)) continue;
                DateTime? at = StampInName().Match(label) is { Success: true } s
                    && DateTime.TryParseExact(s.Value, "yyyyMMdd'T'HHmmss'Z'", CultureInfo.InvariantCulture, DateTimeStyles.AdjustToUniversal | DateTimeStyles.AssumeUniversal, out var when) ? when : null;
                backups.Add(new SaveSource(Key("archive", revision.Path), SaveSourceKind.Backup, Profile("archive-" + label, xuid, revision), player)
                    { BackupAtUtc = at, BackupOf = Whose(xuid, owners, out var gamertag), Gamertag = gamertag });
            }
        }
        return [.. profiles.OrderByDescending(p => p.Save.LastPlayedUtc), .. backups.OrderByDescending(b => b.Save.LastPlayedUtc)];

        bool New(LocalRevision revision) => CommitHash(revision) is { } hash && seen.Add(hash);
    }

    /// <summary>
    /// A source in the player's words (en, pt-BR): "HARDCORE · name", "Old Noob save · name", "Unknown profile on this PC", or "Backup from
    /// {date}" with whose save it copied. Never a folder name (a <c>User_&lt;hex&gt;</c> carries a XUID).
    /// </summary>
    public static string Title(Strings s, SaveSource source)
    {
        ArgumentNullException.ThrowIfNull(s);
        ArgumentNullException.ThrowIfNull(source);
        string Who(SaveSourceKind? kind, string? name) => kind switch
        {
            SaveSourceKind.Hardcore => s.Format("othersave.src_hardcore", ("name", name ?? "?")),
            SaveSourceKind.OldNoob => s.Format("othersave.src_old_noob", ("name", name ?? "?")),
            SaveSourceKind.Azimuth => s["othersave.whose_azimuth"],
            _ => s["othersave.src_unknown"],
        };
        if (source.Kind != SaveSourceKind.Backup) return Who(source.Kind, source.Gamertag);
        var culture = CultureInfo.GetCultureInfo(s.Locale.StartsWith("pt", StringComparison.OrdinalIgnoreCase) ? "pt-BR" : "en-US");
        var when = source.BackupAtUtc is { } at ? at.ToLocalTime().ToString("g", culture) : s["sm.unknown"];
        var backup = s.Format("othersave.src_backup", ("when", when));
        return source.BackupOf is null ? backup : s.Format("othersave.backup_of", ("backup", backup), ("whose", Who(source.BackupOf, source.Gamertag)));
    }

    private static SaveSourceKind? Whose(ulong? xuid, IReadOnlyDictionary<ulong, SaveOwner> owners, out string? gamertag)
    {
        gamertag = null;
        if (xuid == AzimuthSaves.OfflineXuid) return SaveSourceKind.Azimuth;
        if (xuid is { } x && owners.TryGetValue(x, out var owner)) { gamertag = owner.Gamertag; return owner.Kind; }
        return null;
    }

    private static SaveProfile Profile(string folder, ulong? xuid, LocalRevision revision)
    {
        var (xp, level) = AzimuthSaves.XpAndLevel(revision);
        return new SaveProfile(folder, xuid, revision, File.GetLastWriteTimeUtc(Path.Combine(revision.Path, "commit")), AzimuthSaves.Credits(revision)) { Xp = xp, Level = level };
    }

    private static IEnumerable<string> Folders(string dir, string prefix)
    {
        try { return Directory.Exists(dir) ? [.. Directory.EnumerateDirectories(dir, prefix + "*").Order(StringComparer.OrdinalIgnoreCase)] : []; }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return []; }
    }

    private static ulong? XuidOf(string folder, SaveLayout layout) =>
        folder.StartsWith(layout.UserPrefix, StringComparison.OrdinalIgnoreCase)
        && ulong.TryParse(folder.AsSpan(layout.UserPrefix.Length), NumberStyles.AllowHexSpecifier, CultureInfo.InvariantCulture, out var x) ? x : null;

    /// <summary>The SHA-256 of a revision's commit file (it lists every file's hash: equal commits are equal saves), null when it does not read.</summary>
    public static string? CommitHash(LocalRevision revision)
    {
        ArgumentNullException.ThrowIfNull(revision);
        try { return Convert.ToHexStringLower(SHA256.HashData(File.ReadAllBytes(Path.Combine(revision.Path, "commit")))); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return null; }
    }

    private static string Key(string where, string path) =>
        where + ":" + Convert.ToHexStringLower(SHA256.HashData(Encoding.UTF8.GetBytes(Path.GetFullPath(path).ToUpperInvariant())))[..16];

    private static T? Safe<T>(Func<T?> read) where T : class
    {
        try { return read(); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or SaveException) { return null; }
    }

    private static ulong? Parse(string? xuid) => ulong.TryParse(xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var x) ? x : null;

    [GeneratedRegex(@"-([0-9a-fA-F]{16})(?:-\d+)?$")]
    private static partial Regex RevisionInName();

    [GeneratedRegex(@"\d{8}T\d{6}Z")]
    private static partial Regex StampInName();
}

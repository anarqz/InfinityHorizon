// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Security.Cryptography;
using System.Text;
using System.Text.RegularExpressions;
using Infinity.Core.Api;
using Infinity.Core.Process;
using Infinity.Core.Shell;

namespace Infinity.Core.Saves;

/// <summary>Who a save container belongs to, as the player knows it (never its <c>User_&lt;hex&gt;</c> name, which carries a XUID).</summary>
public enum SaveSourceKind
{
    /// <summary>A HARDCORE profile's own folder.</summary>
    Hardcore,

    /// <summary>An AZIMUTH (wire <c>noob</c>) profile's old folder, from before OD-239 sent Azimuth play to <c>User_9000000000000</c>.</summary>
    OldNoob,

    /// <summary>A folder whose XUID some other local profile or career on this PC knows (another account's, a removed one): its gamertag.</summary>
    Named,

    /// <summary>A folder nothing on this PC names: "Other save · LV · modified".</summary>
    Other,

    /// <summary>A folder the player picked anywhere on the PC ("Pick a folder from your PC…"), checked by <see cref="SaveSources.FromFolder"/>.</summary>
    Folder,

    /// <summary>A copy the launcher kept: its history (<see cref="LocalHistory"/>) or Azimuth's <c>fi-launcher-archive</c>.</summary>
    Backup,

    /// <summary>Only as <see cref="SaveSource.BackupOf"/>: a backup of the Azimuth save itself.</summary>
    Azimuth,
}

/// <summary>A profile that owns save folders on this PC: its kind (<see cref="SaveSourceKind.Hardcore"/>, <see cref="SaveSourceKind.OldNoob"/> or <see cref="SaveSourceKind.Named"/>) and plain name.</summary>
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

    /// <summary>A history backup's: why the launcher made it (the entry's own reason, en and pt-BR).</summary>
    public HistoryReason? Reason { get; init; }

    /// <summary>An archive backup's: what the clone or install archived it as (<c>picked</c>, <c>shadowed</c>, <c>rolled-back-…</c>, another label).</summary>
    public string? ArchiveLabel { get; init; }

    /// <summary>The "Backups" group of the picker (collapsed by default); every other kind is "Profiles on this PC".</summary>
    public bool IsBackup => Kind == SaveSourceKind.Backup;
}

/// <summary>Why a picked folder is not a save of the game (a string key of <c>othersave.refuse.*</c>), or the source it is.</summary>
public sealed record FolderCheck(SaveSource? Source, string? Why, string? OtherGame = null);

/// <summary>
/// Owner 2026-11-11 (T-1111-AZIMUTH, azimuth7/9): every save container of one game on this PC that can become the Azimuth save: each
/// <c>player-1\User_*</c> folder but <c>User_9000000000000</c> itself, every container of the launcher's history entries, and every revision in
/// <c>fi-launcher-archive</c>; plus any folder the player picks (<see cref="FromFolder"/>). Only sealed revisions. Exact duplicates (the same
/// commit file) are listed once: a profile over a backup, the newest backup among backups; never the content the Azimuth save already has.
/// Reads only.
/// </summary>
public static partial class SaveSources
{
    /// <summary>
    /// Who owns which XUID, the most meaningful name first: HARDCORE (any HARDCORE profile's XUID is never called anything else), then the
    /// AZIMUTH profiles' old folders (<see cref="OldSaves.Owners"/>), then any other profile or career this PC or the cached <paramref name="me"/>
    /// knows (another account's local profile, a removed career) by its gamertag.
    /// </summary>
    public static IReadOnlyDictionary<ulong, SaveOwner> Owners(Me? me, IEnumerable<LocalProfile> locals)
    {
        ArgumentNullException.ThrowIfNull(locals);
        var all = locals.ToList();
        var owners = new Dictionary<ulong, SaveOwner>();
        void Add(string? xuid, SaveSourceKind kind, string name) { if (Parse(xuid) is { } x && name.Length > 0) owners.TryAdd(x, new SaveOwner(kind, name)); }
        foreach (var p in me?.Players ?? []) if (p.Mode != "noob") Add(p.Xuid, SaveSourceKind.Hardcore, ProfileName.Plain(p.Gamertag));
        foreach (var p in me?.Profiles ?? []) if (p.Mode != "noob") Add(p.Xuid, SaveSourceKind.Hardcore, ProfileName.Plain(p.Gamertag));
        foreach (var l in all.Where(l => l.Mode != "noob"))
        {
            if (l.DisplayName.Length > 0) owners.TryAdd(l.OfflineXuid, new SaveOwner(SaveSourceKind.Hardcore, l.DisplayName));
            Add(l.Cloud?.Xuid, SaveSourceKind.Hardcore, l.DisplayName);
        }
        foreach (var o in OldSaves.Owners(me, all)) if (o.Gamertag.Length > 0) owners.TryAdd(o.Xuid, new SaveOwner(SaveSourceKind.OldNoob, o.Gamertag));
        // azimuth9: anything else this PC knows names the folder (never the User_ hex)
        foreach (var p in me?.Players ?? []) Add(p.Xuid, SaveSourceKind.Named, ProfileName.Plain(p.Gamertag));
        foreach (var p in me?.Profiles ?? []) Add(p.Xuid, SaveSourceKind.Named, ProfileName.Plain(p.Gamertag));
        foreach (var l in all)
        {
            if (l.DisplayName.Length > 0) owners.TryAdd(l.OfflineXuid, new SaveOwner(SaveSourceKind.Named, l.DisplayName));
            Add(l.Cloud?.Xuid, SaveSourceKind.Named, ProfileName.Plain(l.Cloud?.Gamertag ?? l.DisplayName));
        }
        owners.Remove(AzimuthSaves.OfflineXuid);
        return owners;
    }

    /// <summary>
    /// The saves of <paramref name="game"/> under <paramref name="savesRoot"/> and in <paramref name="history"/>: "Profiles on this PC" first
    /// (HARDCORE, old Noob, named, other; newest first in each), then the backups, newest backup first.
    /// </summary>
    public static IReadOnlyList<SaveSource> Scan(GameDefinition game, string savesRoot, LocalHistory history, IReadOnlyDictionary<ulong, SaveOwner> owners)
    {
        ArgumentNullException.ThrowIfNull(game);
        ArgumentNullException.ThrowIfNull(savesRoot);
        ArgumentNullException.ThrowIfNull(history);
        ArgumentNullException.ThrowIfNull(owners);
        var layout = game.Saves;
        var player = Path.Combine(savesRoot, layout.PlayerDir);
        var azimuthName = layout.UserFolder(AzimuthSaves.OfflineXuid);
        var profiles = new List<SaveSource>();
        var backups = new List<SaveSource>();

        // 1. this PC's profile folders
        foreach (var dir in Folders(player, layout.UserPrefix))
        {
            var name = Path.GetFileName(dir);
            var xuid = XuidOf(name, layout);
            if (xuid == AzimuthSaves.OfflineXuid || Safe(() => AzimuthSaves.NewestSealed(dir)) is not { } revision) continue;
            var owner = xuid is { } x && owners.TryGetValue(x, out var o) ? o : null;
            profiles.Add(new SaveSource(Key("pc", revision.Path), owner?.Kind ?? SaveSourceKind.Other, Profile(name, xuid, revision), player) { Gamertag = owner?.Gamertag });
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
                if (Safe(() => AzimuthSaves.NewestSealed(dir)) is not { } revision) continue;
                var xuid = name is null ? Parse(item.Entry.Profile?.Xuid) ?? (item.Entry.To == "azimuth" ? AzimuthSaves.OfflineXuid : null) : XuidOf(name, layout);
                backups.Add(new SaveSource(Key("history", revision.Path), SaveSourceKind.Backup, Profile("history-" + item.Name, xuid, revision), siblings)
                    { BackupAtUtc = at, BackupOf = Whose(xuid, owners, out var gamertag), Gamertag = gamertag, Reason = item.Entry.Reason });
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
                if (Safe(() => AzimuthSaves.ReadRevision(number, dir)) is not { } revision || Safe(() => AzimuthSaves.Verify(revision)) is not { Count: 0 }) continue;
                var stamp = StampInName().Match(label);
                DateTime? at = stamp.Success
                    && DateTime.TryParseExact(stamp.Value, "yyyyMMdd'T'HHmmss'Z'", CultureInfo.InvariantCulture, DateTimeStyles.AdjustToUniversal | DateTimeStyles.AssumeUniversal, out var when) ? when : null;
                backups.Add(new SaveSource(Key("archive", revision.Path), SaveSourceKind.Backup, Profile("archive-" + label, xuid, revision), player)
                    { BackupAtUtc = at, BackupOf = Whose(xuid, owners, out var gamertag), Gamertag = gamertag, ArchiveLabel = stamp.Success ? label[..stamp.Index].TrimEnd('-') : label });
            }
        }

        // exact duplicates (the same commit file) once: the Azimuth save's own content never; a profile over a backup; the newest backup
        var seen = new HashSet<string>(StringComparer.Ordinal);
        if (Safe(() => AzimuthSaves.NewestSealed(Path.Combine(player, azimuthName))) is { } current && CommitHash(current) is { } now) seen.Add(now);
        bool New(SaveSource s) => CommitHash(s.Save.Revision) is { } hash && seen.Add(hash);
        var keptProfiles = profiles.OrderBy(p => p.Kind).ThenByDescending(p => p.Save.LastPlayedUtc).Where(New).ToList();
        var keptBackups = backups.OrderByDescending(b => b.BackupAtUtc ?? b.Save.LastPlayedUtc).ThenByDescending(b => b.Save.LastPlayedUtc).Where(New).ToList();
        return [.. keptProfiles, .. keptBackups];
    }

    /// <summary>
    /// The first-launch notice for former Noob players (T-1111-AZIMUTH, owner 2026-11-11): only the AZIMUTH profiles' own old folders of
    /// <paramref name="game"/> (<c>player-1\User_&lt;noob XUID&gt;</c> of each of <paramref name="owners"/>, <see cref="OldSaves.Owners"/>: decided
    /// by mode, never a HARDCORE XUID, never <c>User_9000000000000</c>), each as its newest sealed revision. Reads only; no history, no archive.
    /// </summary>
    public static IReadOnlyList<SaveSource> OldNoobFolders(GameDefinition game, string savesRoot, IEnumerable<OldSaveOwner> owners)
    {
        ArgumentNullException.ThrowIfNull(game);
        ArgumentNullException.ThrowIfNull(savesRoot);
        ArgumentNullException.ThrowIfNull(owners);
        var layout = game.Saves;
        var player = Path.Combine(savesRoot, layout.PlayerDir);
        var found = new List<SaveSource>();
        foreach (var owner in owners)
        {
            if (owner.Xuid == AzimuthSaves.OfflineXuid) continue;
            var name = layout.UserFolder(owner.Xuid);
            var dir = Path.Combine(player, name);
            if (!Directory.Exists(dir) || Safe(() => AzimuthSaves.NewestSealed(dir)) is not { } revision) continue;
            found.Add(new SaveSource(Key("pc", revision.Path), SaveSourceKind.OldNoob, Profile(name, owner.Xuid, revision), player) { Gamertag = owner.Gamertag });
        }
        return found;
    }

    /// <summary>A sealed revision as the picker and the AZIMUTH row read it (credits, XP, stored level, modified).</summary>
    public static SaveProfile Read(string folder, ulong? xuid, LocalRevision revision)
    {
        ArgumentNullException.ThrowIfNull(folder);
        ArgumentNullException.ThrowIfNull(revision);
        return Profile(folder, xuid, revision);
    }

    /// <summary>
    /// azimuth9 "Pick a folder from your PC…": <paramref name="folder"/> as a save of <paramref name="game"/>, checked with the game's own rules
    /// before anything is written: a <c>User_*</c> container (not <c>User_9000000000000</c> itself) with <c>revisions\&lt;16 hex&gt;\{commit,
    /// data}</c>, whose newest sealed revision holds the game's required files and only its allowed ones (another game's save says which game).
    /// A torn newest set with nothing sealed is refused as such. Reads only. Its sibling containers (FM5) are looked for beside it.
    /// </summary>
    public static FolderCheck FromFolder(GameDefinition game, string folder)
    {
        ArgumentNullException.ThrowIfNull(game);
        ArgumentNullException.ThrowIfNull(folder);
        var layout = game.Saves;
        string full;
        try { full = Path.TrimEndingDirectorySeparator(Path.GetFullPath(folder)); }
        catch (Exception e) when (e is ArgumentException or NotSupportedException or PathTooLongException) { return new(null, "othersave.refuse.not_save"); }
        var name = Path.GetFileName(full);
        if (string.Equals(name, layout.UserFolder(AzimuthSaves.OfflineXuid), StringComparison.OrdinalIgnoreCase)) return new(null, "othersave.refuse.azimuth_itself");
        if (!name.StartsWith(layout.UserPrefix, StringComparison.OrdinalIgnoreCase) || !Directory.Exists(Path.Combine(full, layout.RevisionsDir)))
            return new(null, "othersave.refuse.not_save");
        IReadOnlyList<(ulong Number, string Path)> folders;
        try { folders = AzimuthSaves.RevisionFolders(full); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return new(null, "othersave.refuse.not_save"); }
        LocalRevision? sealedNewest = null;
        var anyCommitted = false;
        foreach (var (number, path) in folders.Reverse())
        {
            if (AzimuthSaves.ReadRevision(number, path) is not { } revision) continue;
            anyCommitted = true;
            if (AzimuthSaves.Verify(revision).Count == 0) { sealedNewest = revision; break; }
        }
        if (sealedNewest is null) return new(null, anyCommitted ? "othersave.refuse.torn" : "othersave.refuse.no_sealed");
        if (!Fits(layout, sealedNewest))
        {
            var other = GameDefinition.All.FirstOrDefault(g => g.Id != game.Id && Fits(g.Saves, sealedNewest));
            return other is null ? new(null, "othersave.refuse.not_save") : new(null, "othersave.refuse.other_game", other.Name);
        }
        var xuid = XuidOf(name, layout);
        return new(new SaveSource(Key("folder", sealedNewest.Path), SaveSourceKind.Folder, Profile("picked-folder", xuid, sealedNewest),
            Path.GetDirectoryName(full) ?? full), null);
    }

    /// <summary>A revision holds every file the layout requires and none it does not allow.</summary>
    private static bool Fits(SaveLayout layout, LocalRevision revision) =>
        layout.RequiredFiles.All(r => revision.Entries.Any(e => e.Name == r)) && revision.Entries.All(e => layout.AllowedFiles.Contains(e.Name));

    /// <summary>
    /// A source in the player's words (en, pt-BR); never a folder name (a <c>User_&lt;hex&gt;</c> carries a XUID). Profiles: "HARDCORE · name",
    /// "Old Noob save · name", "Save of name", else "Other save · LV x · modified date" (<paramref name="level"/> as the row shows it). Backups:
    /// what they kept and why, "Your Azimuth save, before copying … · date".
    /// </summary>
    public static string Title(Strings s, SaveSource source, string? level = null)
    {
        ArgumentNullException.ThrowIfNull(s);
        ArgumentNullException.ThrowIfNull(source);
        var culture = CultureInfo.GetCultureInfo(s.Locale.StartsWith("pt", StringComparison.OrdinalIgnoreCase) ? "pt-BR" : "en-US");
        string When(DateTime utc) => utc.ToLocalTime().ToString("g", culture);
        string Who(SaveSourceKind? kind, string? name) => kind switch
        {
            SaveSourceKind.Hardcore => s.Format("othersave.src_hardcore", ("name", name ?? "?")),
            SaveSourceKind.OldNoob => s.Format("othersave.src_old_noob", ("name", name ?? "?")),
            SaveSourceKind.Named => s.Format("othersave.src_named", ("name", name ?? "?")),
            SaveSourceKind.Azimuth => s["othersave.b_azimuth"],
            _ => s["othersave.b_unknown"],
        };
        switch (source.Kind)
        {
            case SaveSourceKind.Folder:
                return s["othersave.src_folder"];
            case SaveSourceKind.Other:
                return s.Format("othersave.src_other", ("level", level ?? s["sm.unknown"]), ("when", When(source.Save.LastPlayedUtc)));
            case not SaveSourceKind.Backup:
                return Who(source.Kind, source.Gamertag);
        }
        var whose = Who(source.BackupOf, source.Gamertag);
        var at = When(source.BackupAtUtc ?? source.Save.LastPlayedUtc);
        var why = source.Reason is { } reason ? Lower(reason.For(s.Locale), culture)
            : source.ArchiveLabel is { } label ? s[label.StartsWith("picked", StringComparison.Ordinal) ? "othersave.why_picked"
                : label.StartsWith("shadowed", StringComparison.Ordinal) ? "othersave.why_shadowed"
                : label.StartsWith("rolled-back", StringComparison.Ordinal) ? "othersave.why_rolled_back" : "othersave.why_replaced"]
            : null;
        return why is null ? s.Format("othersave.backup_label_nowhy", ("whose", whose), ("when", at))
            : s.Format("othersave.backup_label", ("whose", whose), ("why", why), ("when", at));
    }

    private static string Lower(string text, CultureInfo culture) => text.Length == 0 ? text : char.ToLower(text[0], culture) + text[1..];

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

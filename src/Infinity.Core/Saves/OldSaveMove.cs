// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Text.Json;
using System.Text.Json.Serialization;
using Infinity.Core.Api;
using Infinity.Core.Shell;

namespace Infinity.Core.Saves;

/// <summary>Where an old save of an AZIMUTH profile is.</summary>
public enum OldSaveSource
{
    /// <summary>The profile's own <c>User_&lt;XUID&gt;</c> folder on this PC (what it played before the hook sent it to Azimuth's save).</summary>
    ThisPc,

    /// <summary>An old Noob cloud line (<c>/me</c> <c>cloud.exportOnly</c>, CR-S111), read-only until it is erased.</summary>
    Cloud,
}

/// <summary>An AZIMUTH profile (wire mode <c>noob</c>) whose old saves are looked for: its XUID and the name shown.</summary>
public sealed record OldSaveOwner(ulong Xuid, string Gamertag);

/// <summary>
/// One old save of an AZIMUTH profile (OD-239): <see cref="SavedAtUtc"/> is its newest revision's time (this PC: the newest sealed revision's
/// commit; the cloud: <c>cloud.savedAt</c> of the cached <c>/me</c>), null when it does not read.
/// </summary>
public sealed record OldSave(OldSaveSource Source, string Game, ulong Xuid, string Gamertag, DateTime? SavedAtUtc, SaveProfile? Local = null, OldNoobLine? Line = null)
{
    /// <summary>A stable key of the row (source, game, XUID).</summary>
    public string Key => string.Create(CultureInfo.InvariantCulture, $"{(Source == OldSaveSource.Cloud ? "cloud" : "pc")}:{Game}:{Xuid:X}");
}

/// <summary>
/// OD-239: an AZIMUTH profile plays Azimuth's own save (<c>User_9000000000000</c>, the hook redirects the game there). What it played before
/// stays where it was: the profile's own <c>User_&lt;XUID&gt;</c> folder on this PC and, for an account profile, the old Noob cloud line. This finds
/// them, and decides the launcher-open prompt: shown when one of them is newer than that game's Azimuth save. Reads files only; the cloud's
/// times come from the cached <c>/me</c> (no request).
/// </summary>
public static class OldSaves
{
    /// <summary>
    /// Every XUID that is (or this PC or the account ever said is) a HARDCORE profile's: the account's Hardcore players and careers in any
    /// status, and each Hardcore local profile's own offline XUID and account XUID (a stale cache included). Never an old-save source, whatever
    /// else says so (OD-239 BUG A: a Hardcore save was moved into the Azimuth save).
    /// </summary>
    public static IReadOnlySet<ulong> HardcoreXuids(Me? me, IEnumerable<LocalProfile> locals)
    {
        ArgumentNullException.ThrowIfNull(locals);
        var hardcore = new HashSet<ulong>();
        foreach (var p in me?.Players ?? [])
        {
            if (p.Mode != "noob" && Parse(p.Xuid) is { } x) hardcore.Add(x);
        }
        foreach (var p in me?.Profiles ?? [])
        {
            if (p.Mode != "noob" && Parse(p.Xuid) is { } x) hardcore.Add(x);
        }
        foreach (var l in locals.Where(l => l.Mode != "noob"))
        {
            hardcore.Add(l.OfflineXuid);
            if (Parse(l.Cloud?.Xuid) is { } x) hardcore.Add(x);
        }
        return hardcore;
    }

    /// <summary>
    /// The AZIMUTH profiles whose own old folders are looked at, decided by MODE only: the account's live Noob players and careers
    /// (<c>players</c> and <c>profiles</c> of <paramref name="me"/>) and this PC's Noob local profiles (each one's own offline XUID and, on the
    /// account, its account XUID; signed in, only those not of another account). Never a XUID in <see cref="HardcoreXuids"/> (of
    /// <paramref name="me"/>, <paramref name="locals"/> and <paramref name="hardcore"/>), never Azimuth's own player.
    /// </summary>
    public static IReadOnlyList<OldSaveOwner> Owners(Me? me, IEnumerable<LocalProfile> locals, IReadOnlySet<ulong>? hardcore = null)
    {
        ArgumentNullException.ThrowIfNull(locals);
        var all = locals.ToList();
        var never = new HashSet<ulong>(HardcoreXuids(me, all)) { AzimuthSaves.OfflineXuid };
        if (hardcore is not null) never.UnionWith(hardcore);
        var owners = new Dictionary<ulong, string>();
        void Add(ulong? xuid, string name) { if (xuid is { } x && !never.Contains(x)) owners.TryAdd(x, name); }
        foreach (var p in me?.Players ?? [])
        {
            if (p is { Mode: "noob", Status: "active" }) Add(Parse(p.Xuid), ProfileName.Plain(p.Gamertag));
        }
        foreach (var p in me?.Profiles ?? [])
        {
            if (p is { Mode: "noob", Status: "active" }) Add(Parse(p.Xuid), ProfileName.Plain(p.Gamertag));
        }
        foreach (var l in all.Where(l => l.Mode == "noob" && (me is null || l.LocalOnly || l.OnAccount(me.AccountId))))
        {
            Add(l.OfflineXuid, l.DisplayName); // its own local folder (what it played before OD-239 sent it to Azimuth's save)
            if (!l.NotOnAccount) Add(Parse(l.Cloud?.Xuid), l.DisplayName); // and its account folder
        }
        return [.. owners.Select(o => new OldSaveOwner(o.Key, o.Value))];
    }

    /// <summary>Each owner's folder of <paramref name="game"/> on this PC that holds a sealed revision.</summary>
    public static IReadOnlyList<OldSave> OnThisPc(string game, string savesRoot, IEnumerable<OldSaveOwner> owners, IReadOnlySet<ulong>? hardcore = null)
    {
        ArgumentNullException.ThrowIfNull(game);
        ArgumentNullException.ThrowIfNull(savesRoot);
        ArgumentNullException.ThrowIfNull(owners);
        var found = new List<OldSave>();
        foreach (var owner in owners)
        {
            if (owner.Xuid == AzimuthSaves.OfflineXuid || hardcore?.Contains(owner.Xuid) == true) continue;
            LocalRevision? newest;
            try { newest = AzimuthSaves.NewestSealed(CloudSaveFiles.ProfileDir(savesRoot, owner.Xuid)); }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException) { newest = null; }
            if (newest is null) continue;
            var at = RevisionTime(newest);
            found.Add(new OldSave(OldSaveSource.ThisPc, game, owner.Xuid, owner.Gamertag, at,
                new SaveProfile(AzimuthSaves.FolderFor(owner.Xuid), owner.Xuid, newest, at, AzimuthSaves.Credits(newest))));
        }
        return found;
    }

    /// <summary>The old Noob cloud lines (<see cref="OldNoobExport.Lines"/>) as old saves, their time from <c>cloud.savedAt</c>.</summary>
    public static IReadOnlyList<OldSave> InTheCloud(IEnumerable<OldNoobLine> lines, IReadOnlySet<ulong>? hardcore = null)
    {
        ArgumentNullException.ThrowIfNull(lines);
        return [.. lines.Where(l => Parse(l.Xuid) is { } x && x != AzimuthSaves.OfflineXuid && hardcore?.Contains(x) != true)
            .Select(l => new OldSave(OldSaveSource.Cloud, l.Game, Parse(l.Xuid)!.Value, ProfileName.Plain(l.Gamertag), Time(l.SavedAt), Line: l))];
    }

    /// <summary>The time of Azimuth's own save of a game (<c>User_9000000000000</c>'s newest sealed revision), null when there is none.</summary>
    public static DateTime? AzimuthNewest(string savesRoot)
    {
        ArgumentNullException.ThrowIfNull(savesRoot);
        try
        {
            return AzimuthSaves.NewestSealed(CloudSaveFiles.ProfileDir(savesRoot, AzimuthSaves.OfflineXuid)) is { } newest ? RevisionTime(newest) : null;
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return null; }
    }

    /// <summary>A revision's time: when its commit file was written (what the save picker shows as "last played").</summary>
    public static DateTime RevisionTime(LocalRevision revision)
    {
        ArgumentNullException.ThrowIfNull(revision);
        return File.GetLastWriteTimeUtc(Path.Combine(revision.Path, "commit"));
    }

    /// <summary>
    /// The launcher-open rule (OD-239): per game, the newest old save against that game's Azimuth save (<paramref name="azimuthNewest"/>, by
    /// game id). True when one is newer, or when the game has no Azimuth save at all. An old save whose time does not read only counts against
    /// a missing Azimuth save.
    /// </summary>
    public static bool AnyNewer(IEnumerable<OldSave> saves, Func<string, DateTime?> azimuthNewest)
    {
        ArgumentNullException.ThrowIfNull(saves);
        ArgumentNullException.ThrowIfNull(azimuthNewest);
        foreach (var game in saves.GroupBy(s => s.Game, StringComparer.Ordinal))
        {
            var newest = game.Max(s => s.SavedAtUtc ?? DateTime.MinValue);
            if (azimuthNewest(game.Key) is not { } azimuth || newest > azimuth.ToUniversalTime()) return true;
        }
        return false;
    }

    private static ulong? Parse(string? xuid) => ulong.TryParse(xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var x) ? x : null;

    private static DateTime? Time(string? at) => DateTimeOffset.TryParse(at, CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal, out var t) ? t.UtcDateTime : null;
}

/// <summary>
/// The launcher-open prompt's "Dismiss", per account (<c>old-save-prompt.json</c> in the launcher's state folder): once dismissed it is never
/// shown again for that account on this PC. A missing or unreadable file is "never dismissed"; it is written whole through a temporary file.
/// </summary>
public sealed class OldSavePromptStore(string stateDirectory)
{
    public const string FileName = "old-save-prompt.json";

    private static readonly JsonSerializerOptions Json = new(JsonSerializerDefaults.Web) { WriteIndented = true };

    private sealed record Document(int SchemaVersion, List<string> Dismissed);

    private string File_ => Path.Combine(stateDirectory, FileName);

    public bool IsDismissed(string account)
    {
        ArgumentNullException.ThrowIfNull(account);
        return Read().Contains(account, StringComparer.Ordinal);
    }

    public void Dismiss(string account)
    {
        ArgumentNullException.ThrowIfNull(account);
        var list = Read();
        if (list.Contains(account, StringComparer.Ordinal)) return;
        list.Add(account);
        Directory.CreateDirectory(stateDirectory);
        var temp = File_ + ".tmp";
        File.WriteAllBytes(temp, JsonSerializer.SerializeToUtf8Bytes(new Document(1, list), Json));
        File.Move(temp, File_, overwrite: true);
    }

    private List<string> Read()
    {
        try
        {
            if (!File.Exists(File_) || new FileInfo(File_).Length > 64 * 1024) return [];
            return JsonSerializer.Deserialize<Document>(File.ReadAllBytes(File_), Json) is { SchemaVersion: 1, Dismissed: { } list } ? [.. list] : [];
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or JsonException) { return []; }
    }
}

/// <summary>How a move ended.</summary>
public enum OldSaveMoveOutcome
{
    /// <summary>The old save is the newest revision of Azimuth's save; what it held is in <see cref="OldSaveMoveResult.BackupPath"/>.</summary>
    Moved,

    /// <summary>The game runs: nothing was requested or written.</summary>
    GameRunning,

    /// <summary>The old save holds no revision any more: nothing was written.</summary>
    NoSave,

    /// <summary>Azimuth's save could not be backed up (verified): nothing was written.</summary>
    BackupFailed,

    /// <summary>The download, its checks or the install failed (<see cref="OldSaveMoveResult.Code"/>); a backup made before it is named.</summary>
    Failed,

    /// <summary>The source is not an AZIMUTH profile's (a HARDCORE profile's, or Azimuth's own): nothing was read or written.</summary>
    NotAzimuth,
}

/// <summary>
/// What <see cref="OldSaveMove"/> did. <see cref="BackupPath"/>: the dated backup's <c>saves</c> folder, once made, holding one folder per
/// container the install writes (<c>User_9000000000000</c>, and for FM5 each <c>BaseLivery_*</c>/<c>GarageThumbnail_*</c> the bundle carries);
/// <see cref="BackupFolders"/>: those of them that held something, each a full path.
/// </summary>
public sealed record OldSaveMoveResult(OldSaveMoveOutcome Outcome, string? BackupPath = null, ulong? Revision = null, string? Code = null, string? Message = null)
{
    public IReadOnlyList<string> BackupFolders { get; init; } = [];
}

/// <summary>A verified backup: its <c>saves</c> folder and the container folders it holds.</summary>
public sealed record OldSaveBackup(string Path, IReadOnlyList<string> Folders);

/// <summary>
/// OD-239 "Move to my Azimuth save": an old save of an AZIMUTH profile becomes the newest revision of Azimuth's own save of its game.
/// <list type="number">
/// <item>Refused while the game runs (<paramref name="gameRunning"/>, asked again right before any write).</item>
/// <item>A cloud save is first downloaded and verified (SAVES_API.md §4.2: <c>revisions?limit=1</c>, then its bundle).</item>
/// <item>Every container folder the install writes, WHOLE (<c>User_9000000000000</c> and, for a bundle with siblings such as FM5's, each
/// <c>BaseLivery_*</c>/<c>GarageThumbnail_*</c> it carries, <see cref="ICareerBundle.Containers"/>: every revision folder, torn or not, and any
/// other file) is copied into a dated entry of the launcher's history (<see cref="LocalHistory.ArchiveContainers"/>, every file verified by
/// hash), outside Azimuth's folder; its path is known and reported
/// before anything is written. A failed copy writes nothing. (The install's own <c>fi-launcher-archive\…\shadowed-*</c> copies only the
/// 16-hex revision folders.)</item>
/// <item>Then the install: <see cref="AzimuthSaves.Clone"/> from this PC's folder, or the career bundle's install of the download.</item>
/// </list>
/// The source is only read: the old folder and the cloud line are never changed.
/// </summary>
public sealed class OldSaveMove(string savesRoot, LocalHistory history, Func<bool> gameRunning, IReadOnlySet<ulong> hardcore, Func<DateTime>? utcNow = null)
{
    /// <summary>The history entry's reason (en, pt-BR).</summary>
    public static readonly HistoryReason Reason = new("Before moving an old save into the Azimuth save", "Antes de mover um save antigo para o save do Azimuth");

    private readonly string _savesRoot = savesRoot ?? throw new ArgumentNullException(nameof(savesRoot));
    private readonly LocalHistory _history = history ?? throw new ArgumentNullException(nameof(history));
    private readonly Func<bool> _gameRunning = gameRunning ?? throw new ArgumentNullException(nameof(gameRunning));
    private readonly Func<DateTime> _now = utcNow ?? (() => DateTime.UtcNow);

    /// <summary>OD-239 BUG A: every HARDCORE XUID (<see cref="OldSaves.HardcoreXuids"/>), refused as a source before anything is read.</summary>
    private readonly IReadOnlySet<ulong> _hardcore = hardcore ?? throw new ArgumentNullException(nameof(hardcore));

    /// <summary>Only an AZIMUTH profile's own old folder or line moves: never a HARDCORE one, never Azimuth's own save.</summary>
    private bool NotAzimuth(ulong xuid) => xuid == AzimuthSaves.OfflineXuid || _hardcore.Contains(xuid);

    /// <summary>Azimuth's own save of this game: <c>&lt;saves&gt;\player-1\User_9000000000000</c>.</summary>
    public string AzimuthFolder => CloudSaveFiles.ProfileDir(_savesRoot, AzimuthSaves.OfflineXuid);

    /// <summary>The player folder every container lives in (<c>&lt;saves&gt;\player-1</c>).</summary>
    private string PlayerDir => Path.GetDirectoryName(AzimuthFolder)!;

    /// <summary>
    /// The dated backup of every whole container folder in <paramref name="containers"/> (names in <c>player-1</c>; a missing one holds
    /// nothing, none at all is an empty entry), verified file by file; returns the folder holding the copy
    /// (<c>&lt;history&gt;\&lt;yyyyMMddTHHmmssZ&gt;-copy-old-save-to-azimuth\saves</c>, one <c>&lt;container&gt;</c> folder each) and the
    /// container folders it holds. Throws <see cref="HistoryException"/> when it does not verify (nothing left behind).
    /// </summary>
    public OldSaveBackup BackUp(IReadOnlyList<string> containers, HistoryProfile? profile = null, HistoryReason? reason = null)
    {
        ArgumentNullException.ThrowIfNull(containers);
        var now = _now();
        _history.EnsureOriginal(_savesRoot, now); // §7.2: the title's saves as first found, once
        var item = _history.ArchiveContainers("copy", PlayerDir, containers, reason ?? Reason, now, from: "old-save", to: "azimuth", profile, slug: "old save to azimuth");
        var saves = Path.Combine(item.Folder, LocalHistory.SavesDir);
        return new OldSaveBackup(saves, [.. containers.Select(c => Path.Combine(saves, c)).Where(Directory.Exists)]);
    }

    /// <summary>
    /// This PC's old folder of <paramref name="sourceXuid"/> (its newest sealed revision, read now) onto Azimuth's save. <paramref name="backedUp"/>
    /// receives the backup's path before the install starts. The clone writes the <c>User_9000000000000</c> folder only (the old career's
    /// siblings are already in this <c>player-1</c>), so that is what is backed up.
    /// </summary>
    public OldSaveMoveResult MoveFromThisPc(ulong sourceXuid, Action<string>? backedUp = null, HistoryProfile? profile = null, HistoryReason? reason = null)
    {
        if (sourceXuid == AzimuthSaves.OfflineXuid) return new(OldSaveMoveOutcome.Failed, Code: "LOCAL.SAME_FOLDER", Message: "the Azimuth save cannot be moved onto itself");
        if (NotAzimuth(sourceXuid)) return new(OldSaveMoveOutcome.NotAzimuth, Code: "LOCAL.NOT_AZIMUTH", Message: AzimuthSaves.FolderFor(sourceXuid) + " is a HARDCORE profile's");
        if (_gameRunning()) return new(OldSaveMoveOutcome.GameRunning);
        LocalRevision? newest;
        try { newest = AzimuthSaves.NewestSealed(CloudSaveFiles.ProfileDir(_savesRoot, sourceXuid)); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return new(OldSaveMoveOutcome.Failed, Code: "LOCAL.READ_FAILED", Message: e.Message); }
        if (newest is null) return new(OldSaveMoveOutcome.NoSave);
        var (backup, why) = BackUpOrWhy([AzimuthSaves.FolderFor(AzimuthSaves.OfflineXuid)], backedUp, profile, reason);
        if (why is not null) return why;
        if (_gameRunning()) return With(new(OldSaveMoveOutcome.GameRunning), backup);
        try
        {
            var at = OldSaves.RevisionTime(newest);
            var result = AzimuthSaves.Clone(_savesRoot, new SaveProfile(AzimuthSaves.FolderFor(sourceXuid), sourceXuid, newest, at, null), AzimuthSaves.OfflineXuid, _now());
            return With(new(OldSaveMoveOutcome.Moved, Revision: result.Number), backup);
        }
        catch (Exception e) when (e is SaveException or IOException or UnauthorizedAccessException)
        {
            return With(new(OldSaveMoveOutcome.Failed, Code: "LOCAL.WRITE_FAILED", Message: e.Message), backup);
        }
    }

    private static OldSaveMoveResult With(OldSaveMoveResult result, OldSaveBackup? backup) =>
        backup is null ? result : result with { BackupPath = backup.Path, BackupFolders = backup.Folders };

    /// <summary>
    /// The old cloud line's newest revision onto Azimuth's save: downloaded and verified first (<paramref name="bundle"/>: the game's format),
    /// then the backup (<paramref name="backedUp"/> receives its path), then the install. Network errors propagate (nothing written then).
    /// </summary>
    public async Task<OldSaveMoveResult> MoveFromCloudAsync(ISaveApi api, string access, OldNoobLine line, ICareerBundle bundle, Action<string>? backedUp = null,
        HistoryProfile? profile = null, HistoryReason? reason = null, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(api);
        ArgumentNullException.ThrowIfNull(line);
        ArgumentNullException.ThrowIfNull(bundle);
        if (!ulong.TryParse(line.Xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var lineXuid) || NotAzimuth(lineXuid))
            return new(OldSaveMoveOutcome.NotAzimuth, Code: "LOCAL.NOT_AZIMUTH", Message: "the cloud line " + line.Xuid + " is not an AZIMUTH profile's");
        if (_gameRunning()) return new(OldSaveMoveOutcome.GameRunning);
        var page = await api.RevisionsAsync(access, line.Xuid, null, 1, cancellation).ConfigureAwait(false);
        if (!page.Ok || page.Value is null)
            return new(OldSaveMoveOutcome.Failed, Code: page.Error?.Code ?? "HTTP.EMPTY", Message: page.Error?.Message);
        if (page.Value.Items is not [var newest, ..]) return new(OldSaveMoveOutcome.NoSave);
        var download = await api.BundleAsync(access, line.Xuid, newest.Revision, cancellation).ConfigureAwait(false);
        if (!download.Ok || download.Value is null)
            return new(OldSaveMoveOutcome.Failed, Revision: newest.Revision, Code: download.Error?.Code ?? "HTTP.EMPTY", Message: download.Error?.Message);
        var (valid, failure) = CloudSaves.VerifyDownload(download.Value, newest.Revision, newest.ContentSha256, bundle);
        if (valid is null || failure is not null)
            return new(OldSaveMoveOutcome.Failed, Revision: newest.Revision, Code: failure?.Code ?? "SAVES.INTEGRITY", Message: failure?.Message ?? "the bundle was not read");
        if (_gameRunning()) return new(OldSaveMoveOutcome.GameRunning, Revision: newest.Revision);
        // every container the game's bundle will write (FM5: the profile and each sibling it carries), backed up before the install
        var (backup, why) = BackUpOrWhy(bundle.Containers(valid, AzimuthSaves.OfflineXuid), backedUp, profile, reason);
        if (why is not null) return why with { Revision = newest.Revision };
        if (_gameRunning()) return With(new(OldSaveMoveOutcome.GameRunning, Revision: newest.Revision), backup);
        try
        {
            var installed = bundle.Install(_savesRoot, AzimuthSaves.OfflineXuid, valid, valid.ContentSha256, Guid.CreateVersion7().ToString("N"), "before-old-save-move", null);
            return With(new(OldSaveMoveOutcome.Moved, Revision: installed.Number), backup);
        }
        catch (Exception e) when (e is SaveException or IOException or UnauthorizedAccessException)
        {
            return With(new(OldSaveMoveOutcome.Failed, Revision: newest.Revision, Code: "LOCAL.WRITE_FAILED", Message: e.Message), backup);
        }
    }

    /// <summary>The backup (its path handed to <paramref name="backedUp"/> first), or why there is none.</summary>
    private (OldSaveBackup? Backup, OldSaveMoveResult? Failure) BackUpOrWhy(IReadOnlyList<string> containers, Action<string>? backedUp, HistoryProfile? profile, HistoryReason? reason)
    {
        try
        {
            var backup = BackUp(containers, profile, reason);
            backedUp?.Invoke(backup.Path);
            return (backup, null);
        }
        catch (Exception e) when (e is HistoryException or IOException or UnauthorizedAccessException)
        {
            return (null, new(OldSaveMoveOutcome.BackupFailed, Code: "LOCAL.BACKUP_FAILED", Message: e.Message));
        }
    }
}

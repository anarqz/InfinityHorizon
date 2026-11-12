// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Text;
using Infinity.Core.Process;

namespace Infinity.Core.Saves;

/// <summary>Where the old Noob save the notice offers is.</summary>
public enum OldNoobNoticeSource
{
    /// <summary>The AZIMUTH profile's own old folder on this PC (<c>User_&lt;noob XUID&gt;</c>).</summary>
    ThisPc,

    /// <summary>The old Noob cloud line (<c>cloud.exportOnly</c>), read-only until it is erased.</summary>
    Cloud,
}

/// <summary>
/// The head of an old Noob cloud line as <c>GET …/save/revisions?limit=1</c> answers it (SAVES_API §4.2): its revision, content digest
/// (the digest of the files its commit lists) and the summary the server read from it. An <c>exportOnly</c> line never changes, so it is kept
/// in <see cref="OldNoobNoticeStore"/> and read once.
/// </summary>
public sealed record OldNoobCloudHead(ulong Revision, string ContentSha256, long? Credits, long? Xp, long? Level, string? CommittedAt)
{
    public DateTime? CommittedAtUtc => DateTimeOffset.TryParse(CommittedAt, CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal, out var t) ? t.UtcDateTime : null;
}

/// <summary>The old Noob save the notice offers for one game: its source and the numbers shown (LV from <see cref="Xp"/>/<see cref="Level"/> as the AZIMUTH row reads them).</summary>
public sealed record OldNoobCandidate(OldNoobNoticeSource Source, string Gamertag, long? Credits, long? Xp, long? Level, DateTime? SavedAtUtc)
{
    /// <summary>A folder of this PC: what <see cref="OldSaveMove.CopyFrom"/> copies.</summary>
    public SaveSource? Local { get; init; }

    /// <summary>A cloud line: what <see cref="OldSaveMove.MoveFromCloudAsync"/> downloads, verifies and installs.</summary>
    public OldSave? Cloud { get; init; }
}

/// <summary>One game of the notice: the old Noob save offered and this PC's Azimuth save now (null: none yet).</summary>
public sealed record OldNoobNoticeGame(GameDefinition Game, OldNoobCandidate Old, SaveProfile? Azimuth);

/// <summary>
/// T-1111-AZIMUTH (owner first-hand, WINDOWS chat, 2026-11-11; amends OD-240 #4 "no startup message"): a one-time notice for former Noob
/// players, after sign-in or resume. A game is listed only when the account's AZIMUTH profiles (decided by mode, never a HARDCORE source) have
/// an old Noob save of it, an old cloud line with <c>cloud.exportOnly</c> or this PC's <c>User_&lt;noob XUID&gt;</c> folder, whose newest sealed
/// revision differs from that game's <c>User_9000000000000</c> (or there is none). This PC's folders compare by the SHA-256 of their commit file
/// (<see cref="SaveSources.CommitHash"/>); a cloud line, whose commit file comes only with its bundle, by its content digest against the Azimuth
/// save's in the game's bundle format (the digest of the very files that commit lists: equal commits give equal digests). Of several differing
/// old saves of a game, the newest is offered (this PC first on a tie: no download to show it). Reads files only.
/// </summary>
public static class OldNoobNotice
{
    /// <summary>The game's entry, or null when nothing differing is there.</summary>
    public static OldNoobNoticeGame? Decide(GameDefinition game, string savesRoot, IReadOnlyList<OldSaveOwner> owners,
        IEnumerable<(OldSave Save, OldNoobCloudHead Head)> cloud)
    {
        ArgumentNullException.ThrowIfNull(game);
        ArgumentNullException.ThrowIfNull(savesRoot);
        ArgumentNullException.ThrowIfNull(owners);
        ArgumentNullException.ThrowIfNull(cloud);
        var layout = game.Saves;
        var folders = SaveSources.OldNoobFolders(game, savesRoot, owners);
        var lines = cloud.Where(c => c.Save.Game == game.Id && c.Save.Xuid != AzimuthSaves.OfflineXuid).ToList();
        if (folders.Count == 0 && lines.Count == 0) return null; // nothing old of this game: the Azimuth save is not even read
        var azimuthDir = Path.Combine(savesRoot, layout.PlayerDir, layout.UserFolder(AzimuthSaves.OfflineXuid));
        LocalRevision? current = null;
        try { current = Directory.Exists(azimuthDir) ? AzimuthSaves.NewestSealed(azimuthDir) : null; }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or SaveException) { }
        var currentHash = current is null ? null : SaveSources.CommitHash(current);
        string? currentDigest = null;
        var digestRead = false;
        string? Digest()
        {
            if (digestRead) return currentDigest;
            digestRead = true;
            if (current is null) return null;
            try { currentDigest = layout.Bundle.ContentDigest(current); }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException or SaveException or BundleException) { currentDigest = null; }
            return currentDigest;
        }

        var candidates = new List<OldNoobCandidate>();
        foreach (var source in folders)
        {
            if (SaveSources.CommitHash(source.Save.Revision) is not { } hash) continue; // unreadable: never offered
            if (currentHash is not null && string.Equals(hash, currentHash, StringComparison.Ordinal)) continue; // the same save
            candidates.Add(new OldNoobCandidate(OldNoobNoticeSource.ThisPc, source.Gamertag ?? string.Empty, source.Save.Credits, source.Save.Xp, source.Save.Level,
                source.Save.LastPlayedUtc) { Local = source });
        }
        foreach (var (save, head) in lines)
        {
            if (current is not null && Digest() is { } digest && string.Equals(digest, head.ContentSha256, StringComparison.OrdinalIgnoreCase)) continue;
            candidates.Add(new OldNoobCandidate(OldNoobNoticeSource.Cloud, save.Gamertag, head.Credits, head.Xp, head.Level, head.CommittedAtUtc ?? save.SavedAtUtc)
                { Cloud = save });
        }
        if (candidates.Count == 0) return null;
        var offered = candidates.OrderByDescending(c => c.SavedAtUtc ?? DateTime.MinValue).ThenBy(c => c.Source).First();
        return new OldNoobNoticeGame(game, offered, current is null ? null : SaveSources.Read(layout.UserFolder(AzimuthSaves.OfflineXuid), AzimuthSaves.OfflineXuid, current));
    }
}

/// <summary>
/// The notice's state in <c>old-noob-notice</c> of the launcher's state folder: per account, the games whose choice was made (copied or kept:
/// never shown again for that account on this PC), and the old cloud lines' heads already read (an <c>exportOnly</c> line never changes). One
/// line per fact, space-separated: <c>handled &lt;account&gt; &lt;game&gt; &lt;copied|kept&gt;</c>, <c>head &lt;xuid&gt;:&lt;game&gt;:&lt;revision&gt;
/// &lt;revision&gt; &lt;contentSha256&gt; &lt;credits&gt; &lt;xp&gt; &lt;level&gt; &lt;committedAt&gt;</c> (<c>-</c> for an unknown value).
/// </summary>
public sealed class OldNoobNoticeStore(string stateDirectory)
{
    public const string FileName = "old-noob-notice";
    public const string Copied = "copied", Kept = "kept";

    private readonly string _path = Path.Combine(stateDirectory ?? throw new ArgumentNullException(nameof(stateDirectory)), FileName);

    /// <summary>The cache key of an old cloud line's head.</summary>
    public static string HeadKey(OldSave save)
    {
        ArgumentNullException.ThrowIfNull(save);
        return string.Create(CultureInfo.InvariantCulture, $"{save.Xuid}:{save.Game}:{save.Line?.Revision ?? 0}");
    }

    /// <summary>The choice made for <paramref name="game"/> by <paramref name="account"/> on this PC, or null.</summary>
    public string? Handled(string account, string game) =>
        Lines().Where(l => l.Length == 4 && l[0] == "handled" && l[1] == Token(account) && l[2] == Token(game)).Select(l => l[3]).LastOrDefault();

    public bool IsHandled(string account, string game) => Handled(account, game) is not null;

    /// <summary>Keeps the choice (<see cref="Copied"/> or <see cref="Kept"/>). Throws <see cref="IOException"/>/<see cref="UnauthorizedAccessException"/>.</summary>
    public void MarkHandled(string account, string game, string choice)
    {
        if (choice is not (Copied or Kept)) throw new ArgumentOutOfRangeException(nameof(choice));
        if (IsHandled(account, game)) return;
        Append($"handled {Token(account)} {Token(game)} {choice}");
    }

    /// <summary>The head kept for <paramref name="key"/> (<see cref="HeadKey"/>), or null.</summary>
    public OldNoobCloudHead? Head(string key)
    {
        foreach (var l in Lines().Reverse())
        {
            if (l.Length != 9 || l[0] != "head" || l[1] != Token(key)) continue;
            if (!ulong.TryParse(l[2], NumberStyles.None, CultureInfo.InvariantCulture, out var revision) || l[3].Length != 64 || !l[3].All(Uri.IsHexDigit)) return null;
            return new OldNoobCloudHead(revision, l[3], Number(l[4]), Number(l[5]), Number(l[6]), l[7] == "-" ? null : Uri.UnescapeDataString(l[7]));
        }
        return null;
    }

    /// <summary>Keeps a head read from the server. Throws <see cref="IOException"/>/<see cref="UnauthorizedAccessException"/>.</summary>
    public void KeepHead(string key, OldNoobCloudHead head)
    {
        ArgumentNullException.ThrowIfNull(head);
        if (head.ContentSha256.Length != 64 || !head.ContentSha256.All(Uri.IsHexDigit)) return; // never kept as something it is not
        static string N(long? v) => v?.ToString(CultureInfo.InvariantCulture) ?? "-";
        Append(string.Create(CultureInfo.InvariantCulture,
            $"head {Token(key)} {head.Revision} {head.ContentSha256.ToLowerInvariant()} {N(head.Credits)} {N(head.Xp)} {N(head.Level)} {Token(head.CommittedAt ?? "-")} ."));
    }

    private IEnumerable<string[]> Lines()
    {
        try
        {
            if (!File.Exists(_path) || new FileInfo(_path).Length > 256 * 1024) return [];
            return [.. File.ReadAllLines(_path).Select(l => l.Split(' ', StringSplitOptions.RemoveEmptyEntries))];
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return []; }
    }

    private void Append(string line)
    {
        Directory.CreateDirectory(Path.GetDirectoryName(_path)!);
        var text = new StringBuilder();
        if (File.Exists(_path)) text.Append(File.ReadAllText(_path));
        if (text.Length > 0 && text[^1] != '\n') text.Append('\n');
        text.Append(line).Append('\n');
        File.WriteAllText(_path + ".tmp", text.ToString());
        File.Move(_path + ".tmp", _path, overwrite: true);
    }

    /// <summary>One space-free token (ids, game ids and times have none; anything else is percent-escaped).</summary>
    private static string Token(string value) => value.Length == 0 ? "-" : Uri.EscapeDataString(value);

    private static long? Number(string value) => long.TryParse(value, NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture, out var n) ? n : null;
}

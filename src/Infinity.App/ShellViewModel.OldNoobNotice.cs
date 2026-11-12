// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.IO;
using System.Net.Http;
using Infinity.Core.Api;
using Infinity.Core.Process;
using Infinity.Core.Saves;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>One game of the former-Noob notice: the game and the source, LV · CR · saved of both saves, its two choices, then its result.</summary>
public sealed record OldNoobNoticeRow(string Game, string Title, string OldLine, string OldTip, string AzimuthLine, string AzimuthTip)
{
    /// <summary>The choice was made (copied or kept): the buttons go, the result stays until the notice is closed.</summary>
    public bool Decided { get; init; }

    public bool Undecided => !Decided;

    /// <summary>What the last click did (the copy's backup path, a refusal, "kept").</summary>
    public string? Result { get; init; }

    public bool HasResult => !string.IsNullOrEmpty(Result);
}

/// <summary>
/// T-1111-AZIMUTH (owner first-hand, WINDOWS chat, 2026-11-11; amends OD-240 #4 "no startup message"): after sign-in or resume, a one-time
/// notice on PLAY for former Noob players, when one of their old Noob saves differs from this PC's Azimuth save of that game
/// (<see cref="OldNoobNotice.Decide"/>). It names each game and the source, shows LV · CR · saved of both saves as the AZIMUTH row reads them,
/// and offers per game "Copy my Noob save" (the existing <see cref="OldSaveMove"/> paths with the game's bundle: refused while the game runs,
/// a verified backup of the Azimuth save first with its path shown, FM5's siblings included, the source only read) or "Keep Azimuth's save"
/// (writes nothing). A choice is kept per account and game in <see cref="OldNoobNoticeStore"/>: never shown again on this PC. It never blocks
/// PLAY (no overlay, not an <see cref="AwaitingPlayer"/> prompt) and nothing happens without a click. Profiles' entries stay as they are.
/// </summary>
public sealed partial class ShellViewModel
{
    private IReadOnlyList<OldNoobNoticeGame> _noticeGames = [];
    private string? _noticeAccount;

    public ObservableCollection<OldNoobNoticeRow> OldNoobNoticeRows { get; } = [];

    public bool ShowOldNoobNotice => OldNoobNoticeRows.Count > 0 && _session is not null;

    /// <summary>Every listed game has its choice: the notice can be closed.</summary>
    public bool OldNoobNoticeDone => OldNoobNoticeRows.Count > 0 && OldNoobNoticeRows.All(r => r.Decided);

    /// <summary>The notice's choices and the old lines' heads, in the launcher's state folder.</summary>
    private OldNoobNoticeStore NoticeStore => new(_stateDirectory);

    /// <summary><see cref="CheckOldNoobNoticeAsync"/> that never breaks a sign-in: an unexpected error is logged and the notice is skipped this time.</summary>
    private async Task CheckOldNoobNoticeQuietlyAsync(Action? announce)
    {
        try { await CheckOldNoobNoticeAsync(announce); }
        catch (Exception e) when (!IsFatal(e)) { Log("saves: former-Noob notice skipped: " + e.GetType().Name + ": " + e.Message); }
    }

    /// <summary>
    /// Under the sign-in's loading overlay: which games the notice lists. Nothing when every game of the install already has its choice. An old
    /// cloud line's head is read once (<c>revisions?limit=1</c>, kept in the store: an <c>exportOnly</c> line never changes); a failed read is logged
    /// and that line is left out this time (the notice asks again at the next start; Profiles' entries still work).
    /// </summary>
    internal async Task CheckOldNoobNoticeAsync(Action? announce = null)
    {
        ClearOldNoobNotice();
        if (_install is not { } install || _session is not { Me: { } me } session) return;
        var account = me.AccountId;
        var store = NoticeStore;
        var games = GameDefinition.Available(install.Root).Where(g => !store.IsHandled(account, g.Id)).ToList();
        if (games.Count == 0) return;
        IReadOnlyList<LocalProfile> locals;
        try { locals = LocalProfiles.Read(); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { locals = []; }
        var hardcore = OldSaves.HardcoreXuids(me, locals); // OD-239 BUG A: never a HARDCORE source
        var owners = OldSaves.Owners(me, locals, hardcore);
        var lines = OldSaves.InTheCloud(OldNoobExport.Lines(me), hardcore).Where(s => games.Any(g => g.Id == s.Game)).ToList();
        if (owners.Count == 0 && lines.Count == 0) return;
        announce?.Invoke();
        var heads = new List<(OldSave Save, OldNoobCloudHead Head)>();
        foreach (var line in lines)
        {
            var key = OldNoobNoticeStore.HeadKey(line);
            if (store.Head(key) is { } kept) { heads.Add((line, kept)); continue; }
            if (await ReadOldHeadAsync(session, line) is not { } head) continue;
            heads.Add((line, head));
            try { store.KeepHead(key, head); }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException) { Log("saves: old Noob cloud head not kept: " + e.GetType().Name); }
        }
        // one game that does not read never drops the others (logged with its game)
        var found = await Task.Run(() => games.Select(g =>
        {
            try { return OldNoobNotice.Decide(g, AzimuthSaves.SavesRoot(install.Root, g), owners, heads); }
            catch (Exception e) when (!IsFatal(e)) { Log($"saves: former-Noob notice: {g.Id} not checked: {e.GetType().Name}"); return null; }
        }).OfType<OldNoobNoticeGame>().ToList());
        if (_session != session) return; // signed out meanwhile
        _noticeAccount = account;
        _noticeGames = found;
        foreach (var g in found) OldNoobNoticeRows.Add(NoticeRowOf(g));
        Log(found.Count == 0 ? "saves: former-Noob notice: nothing differs from the Azimuth saves"
            : "saves: former-Noob notice for " + string.Join(", ", found.Select(g => g.Game.Id + " (" + g.Old.Source + ")")));
        ChangedNotice();
    }

    /// <summary>The old line's head (§4.2 <c>revisions?limit=1</c>), or null (logged) when it does not read.</summary>
    private async Task<OldNoobCloudHead?> ReadOldHeadAsync(Infinity.Core.Auth.AccountSession session, OldSave line)
    {
        if (GameOf(line.Game) is not { } game || session.Info.Services?.Saves is not { } origin || !Uri.TryCreate(origin, UriKind.Absolute, out var savesOrigin))
        {
            Log($"saves: old Noob cloud save {line.Key} not checked: no saves service");
            return null;
        }
        try
        {
            var api = new SavesApiClient(Http, savesOrigin, ClientHeader, session.CareerGame(game.Id), game.Saves.Bundle);
            using var limit = new CancellationTokenSource(TimeSpan.FromSeconds(10)); // sign-in never waits long on an old line
            var access = await session.Tokens.AccessAsync(limit.Token);
            var page = await api.RevisionsAsync(access, line.Xuid.ToString(System.Globalization.CultureInfo.InvariantCulture), null, 1, limit.Token);
            if (!page.Ok || page.Value is null)
            {
                Log($"saves: old Noob cloud save {line.Key} not checked: {page.Error?.Code ?? "HTTP.EMPTY"}");
                return null;
            }
            if (page.Value.Items is not [var newest, ..]) { Log($"saves: old Noob cloud save {line.Key} holds no revision"); return null; }
            return new OldNoobCloudHead(newest.Revision, newest.ContentSha256, newest.Summary?.Credits, newest.Summary?.Xp, newest.Summary?.Level, newest.CommittedAt);
        }
        catch (Exception e) when (e is HttpRequestException or OperationCanceledException or ApiException or System.Text.Json.JsonException)
        {
            Log($"saves: old Noob cloud save {line.Key} not checked: {e.GetType().Name}");
            return null;
        }
    }

    private OldNoobNoticeRow NoticeRowOf(OldNoobNoticeGame g)
    {
        var old = g.Old;
        var title = S.Format(old.Source == OldNoobNoticeSource.Cloud ? "oldnotice.game_cloud" : "oldnotice.game_pc", ("game", GameName(g.Game.Id)), ("name", old.Gamertag));
        var oldLevel = LocalLevels.Of(g.Game, old.Xp, old.Level, _install?.Root);
        var oldFields = RowFields(oldLevel.Shown?.ToString("N0", SavesCulture), old.Credits,
            old.SavedAtUtc is { } at ? S.Format("oldnotice.saved", ("when", When(at))) : S["sm.unknown"]);
        var oldLine = S.Format("oldnotice.old", ("fields", oldFields));
        string azLine;
        string? azWhy = null;
        if (g.Azimuth is { } az)
        {
            var level = LocalLevels.Of(g.Game, az, _install?.Root);
            azLine = S.Format("oldnotice.azimuth", ("fields", RowFields(level.Shown?.ToString("N0", SavesCulture), az.Credits, S.Format("oldnotice.saved", ("when", When(az.LastPlayedUtc))))));
            azWhy = level.Why;
        }
        else azLine = S["oldnotice.azimuth_none"];
        return new OldNoobNoticeRow(g.Game.Id, title, oldLine, Tip(oldLine, oldLevel.Why), azLine, Tip(azLine, azWhy));
        string Tip(string line, string? why) => why is null ? line : line + "\n" + S.Format("sm.level_why", ("why", S[why]));
    }

    /// <summary>"Copy my Noob save": the game's old save through the existing copy path (backup first, path shown); kept as handled once copied.</summary>
    internal async Task CopyNoobSaveAsync(OldNoobNoticeRow row)
    {
        ArgumentNullException.ThrowIfNull(row);
        if (_noticeCopying || _noticeGames.FirstOrDefault(n => n.Game.Id == row.Game) is not { } item || row.Decided || _session?.Me is not { } me) return;
        _noticeCopying = true; // a double click never starts a second copy
        try { await CopyNoobSaveCoreAsync(row, item, me); }
        finally { _noticeCopying = false; }
    }

    private bool _noticeCopying;

    private async Task CopyNoobSaveCoreAsync(OldNoobNoticeRow row, OldNoobNoticeGame item, Me me)
    {
        _oldSaveResult = null;
        Message = null;
        OldSaveMoveOutcome? outcome;
        if (item.Old.Local is { } source)
        {
            // OD-239 BUG A, asked again now: a XUID that became a HARDCORE profile's is never a source
            IReadOnlyList<LocalProfile> locals;
            try { locals = LocalProfiles.Read(); }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException) { locals = []; }
            if (source.Save.Xuid is not { } x || OldSaves.HardcoreXuids(me, locals).Contains(x))
            {
                SetOldSaveResult(S["oldsave.not_azimuth"]);
                outcome = OldSaveMoveOutcome.NotAzimuth;
            }
            else outcome = await CopyOtherSaveAsync(item.Game, source, SaveSources.Title(S, source));
        }
        else outcome = await MoveOldSaveAsync(new OldSaveRow(item.Old.Cloud!, string.Empty, string.Empty));
        var text = _oldSaveResult ?? Message ?? string.Empty;
        if (outcome == OldSaveMoveOutcome.Moved) text = Record(item.Game.Id, OldNoobNoticeStore.Copied, text);
        Log($"saves: former-Noob notice {item.Game.Id}: copy {outcome?.ToString() ?? "not run"}");
        Replace(row, row with { Decided = outcome == OldSaveMoveOutcome.Moved, Result = text });
    }

    /// <summary>"Keep Azimuth's save": nothing is written (but the choice itself, so the notice never asks again for that game).</summary>
    public void KeepAzimuthSave(OldNoobNoticeRow row)
    {
        ArgumentNullException.ThrowIfNull(row);
        if (row.Decided || !OldNoobNoticeRows.Contains(row)) return;
        var text = Record(row.Game, OldNoobNoticeStore.Kept, S.Format("oldnotice.kept", ("game", GameName(row.Game))));
        Log($"saves: former-Noob notice {row.Game}: kept the Azimuth save");
        Replace(row, row with { Decided = true, Result = text });
    }

    /// <summary>The notice's Close, once every game has its choice.</summary>
    public void CloseOldNoobNotice()
    {
        if (!OldNoobNoticeDone) return;
        ClearOldNoobNotice();
    }

    private void ClearOldNoobNotice()
    {
        _noticeGames = [];
        _noticeAccount = null;
        if (OldNoobNoticeRows.Count == 0) return;
        OldNoobNoticeRows.Clear();
        ChangedNotice();
    }

    /// <summary>Keeps the choice; when the state folder refuses it, the result says so (it may ask again), never silently.</summary>
    private string Record(string game, string choice, string text)
    {
        if (_noticeAccount is not { } account) return text;
        try { NoticeStore.MarkHandled(account, game, choice); return text; }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
            Log("saves: former-Noob notice choice not kept: " + e.GetType().Name);
            return text + " " + S["oldnotice.not_recorded"];
        }
    }

    private void Replace(OldNoobNoticeRow row, OldNoobNoticeRow with)
    {
        var at = OldNoobNoticeRows.IndexOf(row);
        if (at < 0) at = OldNoobNoticeRows.ToList().FindIndex(r => r.Game == row.Game);
        if (at < 0) return;
        OldNoobNoticeRows[at] = with;
        ChangedNotice();
    }

    private void ChangedNotice()
    {
        Changed(nameof(ShowOldNoobNotice));
        Changed(nameof(OldNoobNoticeDone));
    }
}

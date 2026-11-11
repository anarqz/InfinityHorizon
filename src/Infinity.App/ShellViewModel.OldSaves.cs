// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.Globalization;
using System.IO;
using System.Net.Http;
using Infinity.Core.Api;
using Infinity.Core.Process;
using Infinity.Core.Saves;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>One old save in Profiles: what it is, and its actions ("Move to my Azimuth save"; a cloud one also "Export to a file…").</summary>
public sealed record OldSaveRow(OldSave Save, string Title, string Line)
{
    public bool CanExport => Save.Source == OldSaveSource.Cloud;
}

/// <summary>
/// OD-239: an AZIMUTH profile plays Azimuth's own save. Its old saves (the profile's own <c>User_&lt;XUID&gt;</c> folder on this PC, and the
/// account's old Noob cloud line) are listed in Profiles with "Move to my Azimuth save" (<see cref="OldSaveMove"/>: the whole Azimuth save is
/// copied aside first, and its path told); a cloud one keeps "Export to a file…" as the fallback. When the launcher opens, a prompt says so if
/// one of them is newer than that game's Azimuth save (<see cref="OldSaves.AnyNewer"/>), until the player dismisses it for the account
/// (<see cref="OldSavePromptStore"/>). This PC's folders are scanned on a worker; the cloud's times come from the cached <c>/me</c> only.
/// </summary>
public sealed partial class ShellViewModel
{
    private IReadOnlyList<OldSave> _oldLocal = [];
    private bool _oldSavePrompt;
    private string? _oldSavePromptCheckedFor;
    private string? _oldSaveResult;
    private int _oldSaveScan;

    public ObservableCollection<OldSaveRow> OldSaveRows { get; } = [];

    public bool HasOldSaves => OldSaveRows.Count > 0;

    /// <summary>The last move's result (with the backup's path), kept under the list.</summary>
    public string OldSaveResultText => _oldSaveResult ?? string.Empty;

    public bool HasOldSaveResult => !string.IsNullOrEmpty(_oldSaveResult);

    public bool ShowOldSavePrompt => _oldSavePrompt && InShell;

    public string OldSavePromptText => OldSaveRows.Select(r => r.Save).OrderByDescending(s => s.SavedAtUtc ?? DateTime.MinValue).FirstOrDefault() is { } newest
        ? S.Format("oldsave.prompt_body", ("name", newest.Gamertag), ("game", GameName(newest.Game)))
        : string.Empty;

    /// <summary>The account the prompt's "Dismiss" is kept for: the signed-in account, or this PC without one.</summary>
    private string OldSaveAccount => _session?.Me?.AccountId ?? "this-pc";

    /// <summary>The account's old cloud lines of the games this Azimuth install has (cached <c>/me</c>, no request).</summary>
    private IReadOnlyList<OldSave> OldCloudSaves()
    {
        if (_install is not { } install) return [];
        var games = GameDefinition.Available(install.Root).Select(g => g.Id).ToHashSet(StringComparer.Ordinal);
        return [.. OldSaves.InTheCloud(OldNoobLines).Where(s => games.Contains(s.Game))];
    }

    /// <summary>The rows from the last scan of this PC and the cached <c>/me</c> (no file read, no request): every /me and game change.</summary>
    private void FillOldSaveRows()
    {
        var c = CultureInfo.CurrentCulture;
        var rows = _oldLocal.Concat(OldCloudSaves()).Select(s => new OldSaveRow(s,
            S.Format("oldsave.row_title", ("name", s.Gamertag), ("game", GameName(s.Game))),
            s.Source == OldSaveSource.Cloud
                ? S.Format("oldsave.row_cloud", ("rev", s.Line!.Revision.ToString(c)), ("when", s.SavedAtUtc is { } w ? When(w) : S["sm.unknown"]))
                : S.Format("oldsave.row_pc", ("folder", s.Local!.Folder), ("when", s.SavedAtUtc is { } p ? When(p) : S["sm.unknown"])))).ToList();
        if (rows.Select(r => r.Save.Key + "|" + r.Line).SequenceEqual(OldSaveRows.Select(r => r.Save.Key + "|" + r.Line), StringComparer.Ordinal)) return;
        OldSaveRows.Clear();
        foreach (var row in rows) OldSaveRows.Add(row);
        Changed(nameof(HasOldSaves));
        Changed(nameof(OldSavePromptText));
    }

    /// <summary>
    /// This PC's old folders of every game the install has, read on a worker; then the rows. <paramref name="prompt"/> (launcher open): the
    /// prompt is decided once per account and run, unless dismissed.
    /// </summary>
    internal async Task RefreshOldSavesAsync(bool prompt)
    {
        if (_install is not { } install) { _oldLocal = []; FillOldSaveRows(); return; }
        var scan = ++_oldSaveScan;
        var account = OldSaveAccount;
        IReadOnlyList<LocalProfile> locals;
        try { locals = LocalProfiles.Read(); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { locals = []; }
        var owners = OldSaves.Owners(OfflineMode ? null : _session?.Me, locals);
        var games = GameDefinition.Available(install.Root);
        var (local, azimuth) = await Task.Run(() =>
        {
            var found = new List<OldSave>();
            var newest = new Dictionary<string, DateTime?>(StringComparer.Ordinal);
            foreach (var game in games)
            {
                var root = AzimuthSaves.SavesRoot(install.Root, game);
                found.AddRange(OldSaves.OnThisPc(game.Id, root, owners));
                newest[game.Id] = OldSaves.AzimuthNewest(root);
            }
            return ((IReadOnlyList<OldSave>)found, newest);
        });
        if (scan != _oldSaveScan) return; // a newer scan answers
        _oldLocal = local;
        FillOldSaveRows();
        if (!prompt || _oldSavePromptCheckedFor == account) return;
        _oldSavePromptCheckedFor = account;
        var all = local.Concat(OldCloudSaves()).ToList();
        var dismissed = await Task.Run(() => new OldSavePromptStore(_stateDirectory).IsDismissed(account));
        _oldSavePrompt = !dismissed && OldSaves.AnyNewer(all, game => azimuth.GetValueOrDefault(game));
        if (_oldSavePrompt) Log($"saves: an old Azimuth-profile save is newer than the Azimuth save ({all.Count} old saves); asking the player");
        Changed(nameof(ShowOldSavePrompt));
        Changed(nameof(OldSavePromptText));
    }

    /// <summary>The prompt's "Go to PROFILES": closes it (not dismissed: it asks again at the next open while one is newer).</summary>
    public void OldSavePromptGoToProfiles()
    {
        _oldSavePrompt = false;
        OpenProfiles();
        if (ShowSaveManager) SavesHome();
        Changed(nameof(ShowOldSavePrompt));
    }

    /// <summary>The prompt's "Dismiss": never shown again for this account on this PC.</summary>
    public void DismissOldSavePrompt()
    {
        _oldSavePrompt = false;
        try { new OldSavePromptStore(_stateDirectory).Dismiss(OldSaveAccount); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { Log("saves: the prompt's dismissal was not kept: " + e.GetType().Name); }
        Changed(nameof(ShowOldSavePrompt));
    }

    /// <summary>A cloud row's "Export to a file…": the export screen of that line.</summary>
    public void ExportOldSave(OldSaveRow row)
    {
        ArgumentNullException.ThrowIfNull(row);
        if (row.Save.Line is not { } line) return;
        OpenOldNoobExport(line);
    }

    /// <summary>
    /// "Move to my Azimuth save": refused while the game runs; under the busy overlay, the whole Azimuth save of the row's game is copied aside
    /// (dated, verified), then the old save is installed onto it. The result names the backup's exact path; the source is never changed.
    /// </summary>
    internal async Task MoveOldSaveAsync(OldSaveRow row)
    {
        ArgumentNullException.ThrowIfNull(row);
        if (_install is not { } install || GameOf(row.Save.Game) is not { } game) { Message = S["play.setup_needed"]; return; }
        await RefreshGameRunningNowAsync();
        if (MoveGameRunning()) { Message = S["saves.game_running"]; return; }
        var save = row.Save;
        var savesRoot = AzimuthSaves.SavesRoot(install.Root, game);
        var mover = new OldSaveMove(savesRoot, new LocalHistory(_stateRoot, game, Version), MoveGameRunning);
        Uri? savesOrigin = null;
        if (save.Source == OldSaveSource.Cloud
            && (_session?.Info.Services?.Saves is not { } origin || !Uri.TryCreate(origin, UriKind.Absolute, out savesOrigin)))
        {
            Message = S["oldsave.failed_unreachable"];
            return;
        }
        await Run(S["busy.moving_old_save"], async () =>
        {
            OldSaveMoveResult result;
            try
            {
                if (save.Source == OldSaveSource.ThisPc)
                {
                    result = await Task.Run(() => mover.MoveFromThisPc(save.Xuid, path => Log("saves: Azimuth save backed up before the move: " + path)));
                }
                else
                {
                    var session = _session!;
                    var api = new SavesApiClient(Http, savesOrigin!, ClientHeader, session.CareerGame(game.Id), game.Saves.Bundle);
                    var access = await session.Tokens.AccessAsync(CancellationToken.None);
                    result = await Task.Run(() => mover.MoveFromCloudAsync(api, access, save.Line!, game.Saves.Bundle,
                        path => Log("saves: Azimuth save backed up before the move: " + path)));
                }
            }
            catch (Exception e) when (e is HttpRequestException or TaskCanceledException or ApiException)
            {
                Log("saves: old save not moved: " + e.GetType().Name);
                Message = _oldSaveResult = S["oldsave.failed_unreachable"];
                Changed(nameof(OldSaveResultText)); Changed(nameof(HasOldSaveResult));
                return;
            }
            Log($"saves: old save {save.Key} → Azimuth {result.Outcome} r{result.Revision} {result.Code}");
            _oldSaveResult = result.Outcome switch
            {
                OldSaveMoveOutcome.Moved => S.Format("oldsave.moved", ("game", GameName(game.Id)), ("backup", BackupText(result))),
                OldSaveMoveOutcome.GameRunning when result.BackupPath is not null => S.Format("oldsave.stopped_running", ("backup", BackupText(result))),
                OldSaveMoveOutcome.GameRunning => S["saves.game_running"],
                OldSaveMoveOutcome.NoSave => S["oldsave.nothing"],
                OldSaveMoveOutcome.BackupFailed => S.Format("oldsave.backup_failed", ("reason", result.Message ?? result.Code ?? "?")),
                _ when result.BackupPath is not null => S.Format("oldsave.failed", ("reason", result.Message ?? result.Code ?? "?"), ("backup", BackupText(result))),
                _ => S.Format("oldsave.failed_nothing", ("reason", result.Code ?? result.Message ?? "?")),
            };
            Message = _oldSaveResult;
            if (result.Outcome == OldSaveMoveOutcome.Moved) _saveScan.Invalidate();
            Changed(nameof(OldSaveResultText));
            Changed(nameof(HasOldSaveResult));
        });
        await RefreshOldSavesAsync(prompt: false);
        FillSaveCards();
    }

    /// <summary>
    /// The backup as the result names it: its folder, and when it holds more than one container (FM5's liveries and thumbnails beside the
    /// profile) the name of each folder in it.
    /// </summary>
    private string BackupText(OldSaveMoveResult result)
    {
        var path = result.BackupPath ?? string.Empty;
        return result.BackupFolders.Count > 1
            ? S.Format("oldsave.backup_folders", ("path", path), ("folders", string.Join(", ", result.BackupFolders.Select(Path.GetFileName))))
            : path;
    }

    /// <summary>The game runs: the launcher plays it, or azimuth.exe is up (the harness can force it).</summary>
    private bool MoveGameRunning() => SavesForceRunning || Playing || AzimuthRunning();
}

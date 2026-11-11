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

/// <summary>One old Noob save, listed in its game's AZIMUTH row ⋯ on Profiles: a cloud one exports to a file, a folder on this PC opens in Explorer, both copy.</summary>
public sealed record OldSaveRow(OldSave Save, string Title, string Line)
{
    public bool CanExport => Save.Source == OldSaveSource.Cloud;

    public bool CanOpenFolder => Save.Source == OldSaveSource.ThisPc;
}

/// <summary>
/// OD-240 item 5, placed by the prototype (T-1111-AZIMUTH: everything about a save lives on its card): the old Noob saves are items of their
/// game's AZIMUTH row ⋯ on Profiles, listed only while one exists: an old Noob cloud line (<c>cloud.exportOnly</c> on a
/// Noob career of the cached <c>/me</c>) or a <c>User_&lt;XUID&gt;</c> folder on this PC of the account's Noob profiles or this PC's local ones
/// (<see cref="OldSaves.Owners"/>; never a HARDCORE one). Each offers "Export to a file…" (a cloud line, ShellViewModel.OldNoobExport.cs) or "Copy
/// into my Azimuth save" (<see cref="OldSaveMove"/>: the whole Azimuth save is backed up first, verified, and its path told; the old save is only
/// read). No startup message, no row of their own, nothing in Settings. This PC's folders are scanned on a worker; the cloud's lines come from the cached <c>/me</c> only.
/// </summary>
public sealed partial class ShellViewModel
{
    private IReadOnlyList<OldSave> _oldLocal = [];
    /// <summary>OD-239 BUG A: the HARDCORE XUIDs of the last scan (<see cref="OldSaves.HardcoreXuids"/>): never a row or a copy.</summary>
    private IReadOnlySet<ulong> _oldHardcore = new HashSet<ulong>();
    private string? _oldSaveResult;
    private int _oldSaveScan;

    public ObservableCollection<OldSaveRow> OldSaveRows { get; } = [];


    /// <summary>The last copy's or export's result (with the backup's path or the file), kept under the Profiles cards.</summary>
    public string OldSaveResultText => _oldSaveResult ?? string.Empty;

    public bool HasOldSaveResult => !string.IsNullOrEmpty(_oldSaveResult);

    /// <summary>Where an old save is, in the player's words (its folder on this PC, or its old cloud save).</summary>
    private string OldSourceText(OldSave save) => OldSourceText(S, save);

    private static string OldSourceText(Infinity.Core.Strings s, OldSave save) => save.Source == OldSaveSource.Cloud
        ? s["export.source_cloud"]
        : s.Format("export.source_pc", ("folder", AzimuthSaves.FolderFor(save.Xuid)));

    /// <summary>The account's old cloud lines of the games this Azimuth install has (cached <c>/me</c>, no request).</summary>
    private IReadOnlyList<OldSave> OldCloudSaves()
    {
        if (_install is not { } install) return [];
        var games = GameDefinition.Available(install.Root).Select(g => g.Id).ToHashSet(StringComparer.Ordinal);
        return [.. OldSaves.InTheCloud(OldNoobLines, _oldHardcore).Where(s => games.Contains(s.Game))];
    }

    /// <summary>The rows from the last scan of this PC and the cached <c>/me</c> (no file read, no request): every /me and game change.</summary>
    private void FillOldSaveRows()
    {
        var c = CultureInfo.CurrentCulture;
        var rows = _oldLocal.Concat(OldCloudSaves()).Select(s => new OldSaveRow(s,
            S.Format("export.old_row_title", ("name", s.Gamertag), ("game", GameName(s.Game))),
            s.Source == OldSaveSource.Cloud
                ? S.Format("export.old_row_cloud", ("rev", s.Line!.Revision.ToString(c)), ("when", s.SavedAtUtc is { } w ? When(w) : S["sm.unknown"]))
                : S.Format("export.old_row_pc", ("folder", s.Local!.Folder), ("when", s.SavedAtUtc is { } p ? When(p) : S["sm.unknown"])))).ToList();
        if (rows.Select(r => r.Save.Key + "|" + r.Line).SequenceEqual(OldSaveRows.Select(r => r.Save.Key + "|" + r.Line), StringComparer.Ordinal)) return;
        OldSaveRows.Clear();
        foreach (var row in rows) OldSaveRows.Add(row);
        if (ShowSaveManager && _nav.Current == SavesScreen.Home) FillSaveCards(); // the AZIMUTH rows' ⋯ list them
    }

    /// <summary>This PC's old Noob folders of every game the install has, read on a worker; then the rows (the AZIMUTH rows' ⋯ list them while one exists).</summary>
    internal async Task RefreshOldSavesAsync()
    {
        if (_install is not { } install) { _oldLocal = []; FillOldSaveRows(); return; }
        var scan = ++_oldSaveScan;
        IReadOnlyList<LocalProfile> locals;
        try { locals = LocalProfiles.Read(); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { locals = []; }
        // OD-239 BUG A: sources by MODE only; every XUID a HARDCORE profile has (the cached /me too, offline) is never one
        var hardcore = OldSaves.HardcoreXuids(_session?.Me, locals);
        var owners = OldSaves.Owners(OfflineMode ? null : _session?.Me, locals, hardcore);
        var games = GameDefinition.Available(install.Root);
        var local = await Task.Run(() =>
        {
            var found = new List<OldSave>();
            foreach (var game in games) found.AddRange(OldSaves.OnThisPc(game.Id, AzimuthSaves.SavesRoot(install.Root, game), owners, hardcore));
            return (IReadOnlyList<OldSave>)found;
        });
        if (scan != _oldSaveScan) return; // a newer scan answers
        _oldLocal = local;
        _oldHardcore = hardcore;
        FillOldSaveRows();
    }

    /// <summary>An old cloud save's "Export to a file…": the launcher's folder picker for that line; the pick runs the export.</summary>
    public void ExportOldSave(OldSaveRow row)
    {
        ArgumentNullException.ThrowIfNull(row);
        if (row.Save.Line is not { } line) return;
        PickOldNoobExportFolder(line);
    }

    /// <summary>An old folder on this PC's "Open the old folder": Explorer on it (nothing in it is changed).</summary>
    public void OpenOldSaveFolder(OldSaveRow row)
    {
        ArgumentNullException.ThrowIfNull(row);
        if (_install is not { } install || GameOf(row.Save.Game) is not { } game) { Message = S["play.setup_needed"]; return; }
        var folder = CloudSaveFiles.ProfileDir(AzimuthSaves.SavesRoot(install.Root, game), row.Save.Xuid);
        if (!Directory.Exists(folder)) { Message = S.Format("export.folder_gone", ("folder", folder)); return; }
        try { System.Diagnostics.Process.Start(new System.Diagnostics.ProcessStartInfo("explorer.exe", "\"" + folder + "\"") { UseShellExecute = true })?.Dispose(); }
        catch (Exception e) when (e is System.ComponentModel.Win32Exception or InvalidOperationException) { Log("saves: Explorer not opened: " + e.GetType().Name); }
    }

    /// <summary>
    /// "Copy into my Azimuth save": refused while the game runs; under the busy overlay, the whole Azimuth save of the row's game is backed up
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
        // OD-239 BUG A: the HARDCORE XUIDs read again now (this PC's profiles and the cached /me) and refused before anything is read
        IReadOnlyList<LocalProfile> locals;
        try { locals = LocalProfiles.Read(); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { locals = []; }
        var hardcore = new HashSet<ulong>(OldSaves.HardcoreXuids(_session?.Me, locals));
        hardcore.UnionWith(_oldHardcore);
        var mover = new OldSaveMove(savesRoot, new LocalHistory(_stateRoot, game, Version), MoveGameRunning, hardcore);
        var source = OldSourceText(save);
        var profile = new HistoryProfile(save.Xuid.ToString(CultureInfo.InvariantCulture), save.Gamertag, "noob");
        var reason = new HistoryReason(OldReason("en", save), OldReason("pt-BR", save));
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
                    result = await Task.Run(() => mover.MoveFromThisPc(save.Xuid, path => Log("saves: Azimuth save backed up before the copy: " + path), profile, reason));
                }
                else
                {
                    var session = _session!;
                    var api = new SavesApiClient(Http, savesOrigin!, ClientHeader, session.CareerGame(game.Id), game.Saves.Bundle);
                    var access = await session.Tokens.AccessAsync(CancellationToken.None);
                    result = await Task.Run(() => mover.MoveFromCloudAsync(api, access, save.Line!, game.Saves.Bundle,
                        path => Log("saves: Azimuth save backed up before the copy: " + path), profile, reason));
                }
            }
            catch (Exception e) when (e is HttpRequestException or TaskCanceledException or ApiException)
            {
                Log("saves: old save not copied: " + e.GetType().Name);
                SetOldSaveResult(S["oldsave.failed_unreachable"]);
                return;
            }
            Log($"saves: old Noob save {save.Key} → Azimuth {result.Outcome} r{result.Revision} {result.Code}");
            SetOldSaveResult(result.Outcome switch
            {
                OldSaveMoveOutcome.Moved => S.Format("export.copied", ("name", save.Gamertag), ("source", source), ("game", GameName(game.Id)), ("backup", BackupText(result))),
                OldSaveMoveOutcome.NotAzimuth => S.Format("oldsave.not_azimuth", ("folder", AzimuthSaves.FolderFor(save.Xuid))),
                OldSaveMoveOutcome.GameRunning when result.BackupPath is not null => S.Format("oldsave.stopped_running", ("backup", BackupText(result))),
                OldSaveMoveOutcome.GameRunning => S["saves.game_running"],
                OldSaveMoveOutcome.NoSave => S["oldsave.nothing"],
                OldSaveMoveOutcome.BackupFailed => S.Format("oldsave.backup_failed", ("reason", result.Message ?? result.Code ?? "?")),
                _ when result.BackupPath is not null => S.Format("oldsave.failed", ("reason", result.Message ?? result.Code ?? "?"), ("backup", BackupText(result))),
                _ => S.Format("oldsave.failed_nothing", ("reason", result.Code ?? result.Message ?? "?")),
            });
            if (result.Outcome == OldSaveMoveOutcome.Moved) _saveScan.Invalidate();
        });
        await RefreshOldSavesAsync();
        FillSaveCards();
        FillPlayBoard(); // PLAY AZIMUTH reads the Azimuth save again (the cached scan was invalidated)
    }

    /// <summary>The result under the Profiles cards, and the status line.</summary>
    private void SetOldSaveResult(string text)
    {
        Message = _oldSaveResult = text;
        Changed(nameof(OldSaveResultText));
        Changed(nameof(HasOldSaveResult));
    }

    /// <summary>The history entry's reason in <paramref name="locale"/>: the real source (the old Noob profile, where it is).</summary>
    private string OldReason(string locale, OldSave save)
    {
        var s = StringsIn(locale);
        return s.Format("export.copy_reason", ("name", save.Gamertag), ("source", OldSourceText(s, save)));
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

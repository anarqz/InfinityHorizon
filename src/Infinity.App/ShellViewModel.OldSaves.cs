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

/// <summary>One old Noob cloud save, listed in its game's AZIMUTH row ⋯ on Profiles: it exports to a file, or copies into the Azimuth save.</summary>
public sealed record OldSaveRow(OldSave Save, string Title, string Line)
{
    public bool CanExport => Save.Line is not null;
}

/// <summary>
/// OD-240 item 5, placed by the prototype (T-1111-AZIMUTH: everything about a save lives on its card): the old Noob CLOUD saves are items of
/// their game's AZIMUTH row ⋯ on Profiles, listed only while one exists (<c>cloud.exportOnly</c> on a Noob career of the cached <c>/me</c>, never a
/// HARDCORE one): "Export to a file…" (ShellViewModel.OldNoobExport.cs) and "Copy into my Azimuth save" (<see cref="OldSaveMove"/>: downloaded and
/// verified, the Azimuth save backed up first and its path told). An old Noob FOLDER on this PC is one of the sources of "Use another save on
/// this PC…" (ShellViewModel.OtherSave.cs). The lines come from the cached <c>/me</c> only.
/// </summary>
public sealed partial class ShellViewModel
{
    /// <summary>OD-239 BUG A: the HARDCORE XUIDs of the last read (<see cref="OldSaves.HardcoreXuids"/>): never a row or a copy.</summary>
    private IReadOnlySet<ulong> _oldHardcore = new HashSet<ulong>();
    private string? _oldSaveResult;

    public ObservableCollection<OldSaveRow> OldSaveRows { get; } = [];


    /// <summary>The last copy's or export's result (with the backup's path or the file), kept under the Profiles cards.</summary>
    public string OldSaveResultText => _oldSaveResult ?? string.Empty;

    public bool HasOldSaveResult => !string.IsNullOrEmpty(_oldSaveResult);

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
        var rows = OldCloudSaves().Select(s => new OldSaveRow(s,
            S.Format("export.old_row_title", ("name", s.Gamertag), ("game", GameName(s.Game))),
            S.Format("export.old_row_cloud", ("rev", s.Line!.Revision.ToString(c)), ("when", s.SavedAtUtc is { } w ? When(w) : S["sm.unknown"])))).ToList();
        if (rows.Select(r => r.Save.Key + "|" + r.Line).SequenceEqual(OldSaveRows.Select(r => r.Save.Key + "|" + r.Line), StringComparer.Ordinal)) return;
        OldSaveRows.Clear();
        foreach (var row in rows) OldSaveRows.Add(row);
        if (ShowSaveManager && _nav.Current == SavesScreen.Home) FillSaveCards(); // the AZIMUTH rows' ⋯ list them
    }

    /// <summary>The HARDCORE XUIDs (this PC's profiles and the cached <c>/me</c>), then the rows: every /me change.</summary>
    internal Task RefreshOldSavesAsync()
    {
        IReadOnlyList<LocalProfile> locals;
        try { locals = LocalProfiles.Read(); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { locals = []; }
        _oldHardcore = OldSaves.HardcoreXuids(_session?.Me, locals); // OD-239 BUG A: every XUID a HARDCORE profile has is never one
        FillOldSaveRows();
        return Task.CompletedTask;
    }
    /// <summary>An old cloud save's "Export to a file…": the launcher's folder picker for that line; the pick runs the export.</summary>
    public void ExportOldSave(OldSaveRow row)
    {
        ArgumentNullException.ThrowIfNull(row);
        if (row.Save.Line is not { } line) return;
        PickOldNoobExportFolder(line);
    }

    /// <summary>
    /// "Copy into my Azimuth save": refused while the game runs; under the busy overlay, the whole Azimuth save of the row's game is backed up
    /// (dated, verified), then the old save is installed onto it. The result names the backup's exact path; the source is never changed.
    /// </summary>
    internal async Task<OldSaveMoveOutcome?> MoveOldSaveAsync(OldSaveRow row)
    {
        ArgumentNullException.ThrowIfNull(row);
        if (_install is not { } install || GameOf(row.Save.Game) is not { } game) { Message = S["play.setup_needed"]; return null; }
        await RefreshGameRunningNowAsync();
        if (MoveGameRunning()) { Message = S["saves.game_running"]; return OldSaveMoveOutcome.GameRunning; }
        var save = row.Save;
        var savesRoot = AzimuthSaves.SavesRoot(install.Root, game);
        // OD-239 BUG A: the HARDCORE XUIDs read again now (this PC's profiles and the cached /me) and refused before anything is read
        IReadOnlyList<LocalProfile> locals;
        try { locals = LocalProfiles.Read(); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { locals = []; }
        var hardcore = new HashSet<ulong>(OldSaves.HardcoreXuids(_session?.Me, locals));
        hardcore.UnionWith(_oldHardcore);
        var mover = new OldSaveMove(savesRoot, new LocalHistory(_stateRoot, game, Version), MoveGameRunning, hardcore);
        var source = S["export.source_cloud"];
        var profile = new HistoryProfile(save.Xuid.ToString(CultureInfo.InvariantCulture), save.Gamertag, "noob");
        var reason = new HistoryReason(OldReason("en", save), OldReason("pt-BR", save));
        Uri? savesOrigin = null;
        if (save.Line is null
            || (_session?.Info.Services?.Saves is not { } origin || !Uri.TryCreate(origin, UriKind.Absolute, out savesOrigin)))
        {
            Message = S["oldsave.failed_unreachable"];
            return null;
        }
        OldSaveMoveOutcome? outcome = null;
        await Run(S["busy.moving_old_save"], async () =>
        {
            OldSaveMoveResult result;
            try
            {
                var session = _session!;
                var api = new SavesApiClient(Http, savesOrigin!, ClientHeader, session.CareerGame(game.Id), game.Saves.Bundle);
                var access = await session.Tokens.AccessAsync(CancellationToken.None);
                result = await Task.Run(() => mover.MoveFromCloudAsync(api, access, save.Line!, game.Saves.Bundle,
                    path => Log("saves: Azimuth save backed up before the copy: " + path), profile, reason));
            }
            catch (Exception e) when (e is HttpRequestException or TaskCanceledException or ApiException)
            {
                Log("saves: old save not copied: " + e.GetType().Name);
                SetOldSaveResult(S["oldsave.failed_unreachable"]);
                return;
            }
            outcome = result.Outcome;
            Log($"saves: old Noob save {save.Key} → Azimuth {OutcomeWord(result.Outcome)} r{result.Revision} {result.Code}");
            SetOldSaveResult(result.Outcome switch
            {
                OldSaveMoveOutcome.Moved => S.Format("export.copied", ("name", save.Gamertag), ("source", source), ("game", GameName(game.Id)), ("backup", BackupText(result))),
                OldSaveMoveOutcome.NotAzimuth => S["oldsave.not_azimuth"],
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
        return outcome;
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
        return s.Format("export.copy_reason", ("name", save.Gamertag), ("source", s["export.source_cloud"]));
    }

    /// <summary>
    /// The backup as the result names it: its folder (the launcher's history entry). The save folders inside it are named after XUIDs
    /// (<c>User_&lt;hex&gt;</c>), so a notice never lists them; the log does.
    /// </summary>
    private string BackupText(OldSaveMoveResult result)
    {
        if (result.BackupFolders.Count > 0) Log("saves: backup holds " + string.Join(", ", result.BackupFolders.Select(Path.GetFileName)));
        return result.BackupPath ?? string.Empty;
    }

    /// <summary>The outcome in the log's words: a "move" is a copy (the old save is only read), so it is logged as Copied.</summary>
    private static string OutcomeWord(OldSaveMoveOutcome outcome) => outcome == OldSaveMoveOutcome.Moved ? "Copied" : outcome.ToString();

    /// <summary>The game runs: the launcher plays it, or azimuth.exe is up (the harness can force it).</summary>
    private bool MoveGameRunning() => SavesForceRunning || Playing || AzimuthRunning();
}

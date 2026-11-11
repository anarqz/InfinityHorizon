// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.IO;
using Infinity.Core.Process;
using Infinity.Core.Saves;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>One save of the "Use another save on this PC…" picker: who it is (never a <c>User_&lt;hex&gt;</c> name) and LV · CR · modified.</summary>
public sealed record SavePickRow(string Key, string Title, string Line, string LineTip);

/// <summary>
/// Owner 2026-11-11 (first-hand: "the option to put any save to be the new azimuth profile, and use it as their new local profile"; azimuth9
/// readability): the AZIMUTH row's "Use another save on this PC…" opens with "Pick a folder from your PC…" (the launcher's own folder picker,
/// the folder checked as a save of the game before anything is written, <see cref="SaveSources.FromFolder"/>), then "Profiles on this PC"
/// (HARDCORE, old Noob, a gamertag this PC knows, else "Other save · LV · modified") and "Backups" (collapsed, newest first, each saying what
/// it kept and why), exact duplicates once (<see cref="SaveSources.Scan"/>); never <c>User_9000000000000</c> itself. The pick becomes the
/// Azimuth save (<see cref="OldSaveMove.CopyFrom"/>): refused while the game runs, the Azimuth save (and FM5's sibling containers written)
/// backed up first, verified, its path told; the source only read. The scan is kept per game until this PC's saves change.
/// </summary>
public sealed partial class ShellViewModel
{
    private sealed record PickScan(string Game, long Computations, int Epoch, IReadOnlyList<SaveSource> Sources);

    private PickScan? _pickScan;
    private int _pickEpoch;
    private bool _pickBackupsOpen;

    /// <summary>"Profiles on this PC".</summary>
    public ObservableCollection<SavePickRow> PickRows { get; } = [];

    /// <summary>"Backups", newest first; shown only when opened (<see cref="PickBackupsOpen"/>).</summary>
    public ObservableCollection<SavePickRow> PickBackupRows { get; } = [];

    public string PickTitle => S["othersave.title"];

    public string PickSub => S.Format("othersave.sub", ("game", GameLabel));

    public bool PickEmpty => PickRows.Count == 0;

    public bool HasPickBackups => PickBackupRows.Count > 0;

    public bool PickBackupsOpen => _pickBackupsOpen && HasPickBackups;

    public string PickBackupsHeader => S.Format("othersave.group_backups", ("count", PickBackupRows.Count.ToString(SavesCulture)));

    public string PickBackupsToggle => S[_pickBackupsOpen ? "othersave.hide" : "othersave.show"];

    public bool CanUseOtherSave => !_gameRunning && Idle;

    public void TogglePickBackups()
    {
        _pickBackupsOpen = !_pickBackupsOpen;
        Changed(nameof(PickBackupsOpen));
        Changed(nameof(PickBackupsToggle));
    }

    /// <summary>The ⋯ entry: the cached scan when this PC's saves did not change since, else a scan on a worker under its own overlay.</summary>
    internal async Task OpenOtherSaveAsync()
    {
        if (_install is not { } install) { Message = S["play.setup_needed"]; return; }
        var game = CurrentGame;
        if (_pickScan is not { } known || known.Game != game.Id || known.Epoch != _pickEpoch || known.Computations != _saveScan.Computations)
        {
            var root = AzimuthSaves.SavesRoot(install.Root, game);
            IReadOnlyList<LocalProfile> locals;
            try { locals = LocalProfiles.Read(); }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException) { locals = []; }
            var owners = SaveSources.Owners(_session?.Me, locals);
            var history = new LocalHistory(_stateRoot, game, Version);
            await Run(S["busy.finding_saves"], async () =>
            {
                await PrimeLocalFactsQuietlyAsync();
                var sources = await Task.Run(() => SaveSources.Scan(game, root, history, owners));
                _pickScan = new PickScan(game.Id, _saveScan.Computations, _pickEpoch, sources);
                Log($"saves: {sources.Count(s => !s.IsBackup)} other {game.Id} saves and {sources.Count(s => s.IsBackup)} backups found on this PC");
            });
        }
        if (_pickScan is not { } scan || scan.Game != game.Id) return;
        PickRows.Clear();
        PickBackupRows.Clear();
        foreach (var source in scan.Sources) (source.IsBackup ? PickBackupRows : PickRows).Add(PickRowOf(game, source));
        _pickBackupsOpen = false; // collapsed by default
        _oldSaveResult = null; // a refused folder's reason shows on this screen; an older result does not
        Changed(nameof(OldSaveResultText));
        Changed(nameof(HasOldSaveResult));
        RefreshGameRunning();
        SavesGo(SavesScreen.Pick);
        foreach (var name in new[] { nameof(HasPickBackups), nameof(PickBackupsOpen), nameof(PickBackupsHeader), nameof(PickBackupsToggle) }) Changed(name);
    }

    private SavePickRow PickRowOf(GameDefinition game, SaveSource source)
    {
        var level = LocalLevels.Of(game, source.Save, _install?.Root);
        var shown = level.Shown?.ToString("N0", SavesCulture);
        var line = RowFields(shown, source.Save.Credits, S.Format("sm.modified", ("when", When(source.Save.LastPlayedUtc))));
        return new SavePickRow(source.Key, SaveSources.Title(S, source, shown), line, level.Why is { } why ? line + "\n" + S.Format("sm.level_why", ("why", S[why])) : line);
    }

    /// <summary>"Pick a folder from your PC…": the launcher's own folder picker (never the OS dialog), starting at this game's saves.</summary>
    public void PickOtherSaveFolder()
    {
        if (_install is not { } install) { Message = S["play.setup_needed"]; return; }
        var game = CurrentGame;
        OpenPicker(new FilePickRequest(S.Format("othersave.folder_title", ("game", game.Name)), true, false, folder => UseOtherSaveFolderAsync(game, folder))
        {
            Start = Path.Combine(AzimuthSaves.SavesRoot(install.Root, game), game.Saves.PlayerDir),
            PickFolderLabel = S["othersave.folder_pick"],
        });
    }

    /// <summary>The picked folder, checked as a save of <paramref name="game"/> on a worker; a refusal says why and writes nothing.</summary>
    internal async Task UseOtherSaveFolderAsync(GameDefinition game, string folder)
    {
        FolderCheck? check = null;
        await Run(S["busy.checking_save"], async () => check = await Task.Run(() => SaveSources.FromFolder(game, folder)));
        if (check is not { Source: { } source })
        {
            var why = check?.Why ?? "othersave.refuse.not_save";
            Log($"saves: picked folder refused for {game.Id}: {why}"); // no path: it may name the player
            SetOldSaveResult(S.Format(why, ("game", game.Name), ("other", check?.OtherGame ?? "?")));
            ChangedSaves();
            return;
        }
        await CopyOtherSaveAsync(game, source, SaveSources.Title(S, source));
    }

    /// <summary>"Use this save" on a listed row.</summary>
    internal async Task UseOtherSaveAsync(SavePickRow row)
    {
        ArgumentNullException.ThrowIfNull(row);
        if (_pickScan?.Sources.FirstOrDefault(s => s.Key == row.Key) is not { } source) { SavesHome(); return; }
        await CopyOtherSaveAsync(CurrentGame, source, row.Title);
    }

    /// <summary>
    /// Refused while the game runs; under the busy overlay, the Azimuth save backed up (verified) and <paramref name="source"/> copied onto it.
    /// The result names the backup's exact path; the source is never changed.
    /// </summary>
    private async Task CopyOtherSaveAsync(GameDefinition game, SaveSource source, string title)
    {
        if (_install is not { } install) { Message = S["play.setup_needed"]; return; }
        await RefreshGameRunningNowAsync();
        if (MoveGameRunning()) { Message = S["saves.game_running"]; ChangedSaves(); return; }
        var mover = new OldSaveMove(AzimuthSaves.SavesRoot(install.Root, game), new LocalHistory(_stateRoot, game, Version), MoveGameRunning, new HashSet<ulong>());
        var profile = source.Save.Xuid is { } x && source.Kind is SaveSourceKind.Hardcore or SaveSourceKind.OldNoob
            ? new HistoryProfile(x.ToString(System.Globalization.CultureInfo.InvariantCulture), source.Gamertag ?? string.Empty, source.Kind == SaveSourceKind.Hardcore ? "hardcore" : "noob")
            : null;
        // the backup's reason names the source in both languages ("Before copying HARDCORE · Hank into the Azimuth save")
        var reason = new HistoryReason(StringsIn("en").Format("othersave.reason", ("source", SaveSources.Title(StringsIn("en"), source))),
            StringsIn("pt-BR").Format("othersave.reason", ("source", SaveSources.Title(StringsIn("pt-BR"), source))));
        await Run(S["busy.using_other_save"], async () =>
        {
            var result = await Task.Run(() => mover.CopyFrom(source, game.Saves, path => Log("saves: Azimuth save backed up before the copy: " + path), profile, reason));
            Log($"saves: other {game.Id} save {source.Key} ({source.Kind}) → Azimuth {OutcomeWord(result.Outcome)} r{result.Revision} {result.Code}");
            SetOldSaveResult(result.Outcome switch
            {
                OldSaveMoveOutcome.Moved => S.Format("othersave.copied", ("source", title), ("game", GameName(game.Id)), ("backup", BackupText(result))),
                OldSaveMoveOutcome.GameRunning when result.BackupPath is not null => S.Format("oldsave.stopped_running", ("backup", BackupText(result))),
                OldSaveMoveOutcome.GameRunning => S["saves.game_running"],
                OldSaveMoveOutcome.NoSave => S["oldsave.nothing"],
                OldSaveMoveOutcome.BackupFailed => S.Format("oldsave.backup_failed", ("reason", result.Message ?? result.Code ?? "?")),
                _ when result.BackupPath is not null => S.Format("oldsave.failed", ("reason", result.Message ?? result.Code ?? "?"), ("backup", BackupText(result))),
                _ => S.Format("oldsave.failed_nothing", ("reason", result.Code ?? result.Message ?? "?")),
            });
            _pickEpoch++; // the backup and the copy changed what the picker lists
            if (result.Outcome == OldSaveMoveOutcome.Moved) _saveScan.Invalidate();
        });
        SavesHome();
        FillPlayBoard(); // PLAY AZIMUTH reads the Azimuth save again
    }
}

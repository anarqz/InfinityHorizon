// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.IO;
using Infinity.Core.Saves;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>
/// NOOB_FROM_FILE (owner 2026-11-08, Alex via SAURON): "Launcher must accept starting a NOOB profile from a local save file (it syncs with the
/// cloud and starts the game)". PLAY's inline START NOOB CAREER choice and a Noob game row's ⋯ offer "From a save file on this PC…": the
/// in-launcher picker takes a <c>revisions</c> folder or the folder holding it; <see cref="SaveImport.Inspect"/> checks it against the game's
/// layout (catalog data) and says why when it is not a save of the game. On confirm: the Noob career exists (POST .../careers), what this
/// profile's own <c>User_&lt;xuid&gt;</c> folder holds goes to History (§7.1), the save's contents are copied there (sha256-verified, sibling
/// containers only when absent), and Play follows. A game with cloud saves uploads it at that Play (a normal upload); one without them
/// (FM5) says cloud sync comes later. Never while the game runs.
/// </summary>
public sealed partial class ShellViewModel
{
    /// <summary>The Noob card the picked save will start (the career may not exist yet).</summary>
    private BoardCard? _fileCard;

    /// <summary>The checked save waiting for the player's confirmation.</summary>
    private SaveFileSource? _fileSource;

    private string? _fileLastFolder;

    private bool GameRunningNow => Playing || System.Diagnostics.Process.GetProcessesByName("azimuth").Length > 0;

    /// <summary>The confirmation of a checked save (on PLAY).</summary>
    public bool ShowFileStart => _fileSource is not null && _fileCard is not null && ShowHome;

    public string FileStartTitle => _fileCard is not { } card ? string.Empty
        : S.Format("save_file.confirm_title", ("name", ProfileName.Plain(card.Player.Gamertag)), ("game", GameLabel));

    public string FileStartBody => _fileSource is not { } source || _fileCard is not { } card ? string.Empty
        : S.Format("save_file.confirm_body", ("folder", source.ProfileDir), ("rev", source.Revision.Number.ToString("x", CultureInfo.InvariantCulture)),
            ("name", ProfileName.Plain(card.Player.Gamertag)), ("game", GameLabel))
          + (source.Siblings.Count > 0 ? " " + S.Format("save_file.siblings", ("count", source.Siblings.Count.ToString(CultureInfo.CurrentCulture))) : string.Empty)
          + " " + S.Format(SaveImport.CloudNoteKey(CurrentGame), ("game", GameLabel));

    /// <summary>The career names liveries or thumbnails that did not come with the save and are not on this PC (warned, never blocking).</summary>
    public string FileStartWarning => _fileSource is { } source && SavesRootNow is { } root
        && SaveImport.MissingAfterImport(root, CurrentGame.Saves, source) is { Count: > 0 } missing
        ? S.Format("save_file.siblings_missing", ("count", missing.Count.ToString(CultureInfo.CurrentCulture))) : string.Empty;

    public bool HasFileStartWarning => FileStartWarning.Length > 0;

    /// <summary>PLAY's inline choice: "From a save file on this PC…" (a Noob card; Hardcore says why).</summary>
    public bool StartFromFileEnabled => StartCard is { Player.Mode: "noob" } && Idle && !Playing && _install is not null;

    public string StartFromFileHint => CareerStartHardcore ? S["save_file.hint_hardcore"] : S.Format("save_file.hint", ("game", GameLabel));

    /// <summary>PLAY's inline "From a save file on this PC…": the picker opens; Play follows the confirmed copy.</summary>
    public void PlayStartFromFile()
    {
        if (StartCard is not { } card) return;
        _playAfterStart = true;
        AskSaveFile(card);
    }

    /// <summary>A Noob game row's ⋯ "Start from a save file…" (the row's game is current already): the same picker and confirmation, on PLAY.</summary>
    private void StartFromFileOfRow(ProfileRow row)
    {
        if (row.Card is not { } card) return;
        _playAfterStart = true;
        AskSaveFile(card);
    }

    private void AskSaveFile(BoardCard card)
    {
        if (SaveImport.GateKey(GameRunningNow, _install is not null, card.Player.Mode) is { } refused)
        {
            Log($"saves: start {CurrentGame.Id} {card.Player.ProfileId} from a save file refused ({refused})");
            Message = S[refused];
            if (_startRow is null) _playAfterStart = false;
            return;
        }
        _fileCard = card;
        _fileSource = null;
        OpenPicker(new(S.Format("save_file.pick_title", ("game", GameLabel)), true, false, InspectSaveFileAsync)
        {
            Start = _fileLastFolder,
            PickFolderLabel = S["save_file.pick_this"],
            OnCancel = () =>
            {
                _fileCard = null;
                if (_startRow is null) _playAfterStart = false;
                ChangedFileStart();
            },
        });
    }

    /// <summary>The pick: checked against the game's layout (reads only); a refusal says why, a save of the game opens the confirmation.</summary>
    private async Task InspectSaveFileAsync(string folder)
    {
        var layout = CurrentGame.Saves;
        SaveFileCheck check = new(null, new SaveFileRefusal("save_file.not_found"));
        await Run(S["busy.checking_save_file"], async () => check = await Task.Run(() => SaveImport.Inspect(folder, layout)));
        _fileLastFolder = folder;
        if (check.Refusal is { } refusal)
        {
            Log($"saves: save file for {CurrentGame.Id} refused ({refusal.Key}: {refusal.Detail})");
            Message = S.Format("save_file.refused", ("reason", S.Format(refusal.Key, ("detail", refusal.Detail), ("game", GameLabel))));
            _fileCard = null;
            if (_startRow is null) _playAfterStart = false;
            ChangedFileStart();
            return;
        }
        _fileSource = check.Source;
        Log($"saves: save file for {CurrentGame.Id} checked: r{check.Source!.Revision.Number:x16}, {check.Source.Siblings.Count} containers, "
            + $"{check.Source.MissingSiblings.Count} named but absent, {check.Source.Unreferenced.Count} not named (not copied)");
        if (!ShowHome) OpenHome();
        ChangedFileStart();
    }

    public void CancelFileStart()
    {
        _fileCard = null;
        _fileSource = null;
        if (_startRow is null) _playAfterStart = false;
        ChangedFileStart();
    }

    /// <summary>
    /// The confirmation's "Copy and play": the Noob career exists, the profile's own folder is archived to History, the save's contents
    /// are copied into it (sha256-verified), the career is chosen and Play follows. Any step that cannot happen says why and writes nothing
    /// after it.
    /// </summary>
    public async Task ConfirmFileStartAsync()
    {
        if (_fileCard is not { } card || _fileSource is not { } source) return;
        var done = false;
        await Run(S.Format("busy.starting_career", ("mode", BusyMode("noob")), ("game", GameLabel)), async () =>
        {
            if (SaveImport.GateKey(GameRunningNow, _install is not null, card.Player.Mode) is { } refused) { Message = S[refused]; Log("saves: save file copy refused: " + refused); return; }
            var (row, _) = await EnsureCareerAsync(card); // the existing career, or a new one
            if (row is null) return; // the reason is shown
            if (_install is null || !ulong.TryParse(row.Xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var xuid)) { Message = S["play.setup_needed"]; return; }
            var name = ProfileName.Plain(row.Gamertag);
            var root = AzimuthSaves.SavesRoot(_install.Root, CurrentGame);
            // this profile's own folder, never another XUID's; what it holds goes to History first (§7.1)
            var target = CurrentGame.Saves.ProfileDir(root, xuid);
            if (!ArchiveFirst("import", target, "save_file.reason", [("name", "NOOB · " + name), ("game", GameLabel)], "file", "noob"))
            {
                Log("saves: save file copy stopped: History not written");
                return;
            }
            var missing = SaveImport.MissingAfterImport(root, CurrentGame.Saves, source);
            BusyStep(S["busy.copying_save_file"]);
            SaveImportResult result;
            try { result = await Task.Run(() => SaveImport.Import(root, CurrentGame.Saves, source, xuid, DateTime.UtcNow)); }
            catch (Exception e) when (e is SaveException or IOException or UnauthorizedAccessException)
            {
                Log("saves: save file copy failed: " + e.GetType().Name + " " + e.Message);
                Message = S.Format("save_file.failed", ("reason", e.Message));
                return;
            }
            Log($"saves: save file → {result.Career.Target}/{result.Career.Number:x16} ({CurrentGame.Id}); containers copied [{string.Join(", ", result.CopiedSiblings)}], "
                + $"kept [{string.Join(", ", result.SkippedSiblings)}], missing [{string.Join(", ", missing)}]");
            if (!CurrentGame.FullFeatures) Log($"saves: {CurrentGame.Id} keeps no cloud saves yet: the copy stays on this PC");
            SavesChanged();
            _fileCard = null;
            _fileSource = null;
            _startRow = null;
            ChangedCareerStart();
            FillProfiles();
            ChooseProfile(row);
            Message = S.Format("save_file.done", ("name", name), ("game", GameLabel)) + " " + S.Format(SaveImport.CloudNoteKey(CurrentGame), ("game", GameLabel))
                + (missing.Count > 0 ? " " + S.Format("save_file.siblings_missing", ("count", missing.Count.ToString(CultureInfo.CurrentCulture))) : string.Empty);
            done = true;
        });
        ChangedFileStart();
        if (!done) return;
        _playAfterStart = true; // Alex: "it syncs with the cloud and starts the game"
        await PlayAfterStartAsync();
    }

    private void ChangedFileStart()
    {
        Changed(nameof(ShowFileStart));
        Changed(nameof(ShowPlayStart));
        Changed(nameof(FileStartTitle));
        Changed(nameof(FileStartBody));
        Changed(nameof(FileStartWarning));
        Changed(nameof(HasFileStartWarning));
        Changed(nameof(StartFromFileEnabled));
        Changed(nameof(StartFromFileHint));
    }
}

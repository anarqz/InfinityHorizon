// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.Globalization;
using System.IO;
using System.Net.Http;
using Infinity.Core.Api;
using Infinity.Core.Saves;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>One career the restore offers: the archive, its date, and its level and credits.</summary>
public sealed record CareerRestoreRow(FreshStartArchive Archive, string Title, string Detail);

/// <summary>
/// OD-133 "Restore a career from this PC's backups": a Hardcore profile whose career a fresh start moved aside (<c>fresh-start-*</c>)
/// may bring it back, once its line's first save is in the cloud (head ≥ 1). The server is asked first whether the upload would pass
/// (<see cref="ISaveUploadDryRun"/>); only then is the current save backed up, the career copied in and recorded as the next Play's
/// upload (<see cref="CareerRestore"/>). Disabled while the server cannot answer the dry run (MB-133-1). The archives are only read.
/// </summary>
public sealed partial class ShellViewModel
{
    private Profile? _restoreProfile;
    private CareerRestoreRow? _restorePick;
    private int _restoreSkipped;

    public ObservableCollection<CareerRestoreRow> CareerRestoreRows { get; } = [];

    private Profile? ChosenProfile => Profiles.FirstOrDefault(p => p.ProfileId == _gate.ProfileKey);

    /// <summary>The entry shows for the chosen Hardcore profile when this PC holds a career a fresh start moved aside.</summary>
    public bool ShowCareerRestoreEntry
    {
        get
        {
            if (!Ready || _install is null || ChosenProfile is not { Mode: "hardcore" } profile
                || !ulong.TryParse(profile.Xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var xuid)) return false;
            try
            {
                var root = AzimuthSaves.SavesRoot(_install.Root);
                return FreshStartArchives.ArchiveNames(root, xuid).Count > 0
                    || (CloudSaveClient() is { } client && RestoreUndo.Unsettled(client.Saves, root, xuid)); // the way out stays reachable
            }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return false; }
        }
    }

    /// <summary>The server can answer the upload dry run; without it the entry stays disabled (nothing half-wired ships).</summary>
    public bool CareerRestoreSupported => !_restoreUnsupported && CloudSaveClient()?.Api.SupportsUploadDryRun == true;

    /// <summary>This server answered the dry run with 404/405 (before CR-S71): the restore stays off for this launcher run.</summary>
    private bool _restoreUnsupported;

    public bool CanRestoreCareer => CareerRestoreSupported && Ready && _install is not null && !Playing && !Busy;

    /// <summary>Why the entry is disabled, under it (empty when it is not).</summary>
    public string CareerRestoreNote => ShowCareerRestoreEntry && !CareerRestoreSupported ? S["restore.unavailable"] : string.Empty;

    public bool ShowCareerRestore => _restoreProfile is not null;

    public bool ShowCareerRestoreList => _restoreProfile is not null && _restorePick is null;

    public bool ShowCareerRestoreConfirm => _restoreProfile is not null && _restorePick is not null;

    public string CareerRestoreListText => _restoreProfile is not { } p ? string.Empty
        : S.Format("restore.list_text", ("name", ProfileName.Plain(p.Gamertag)))
          + (_restoreSkipped > 0 ? Environment.NewLine + S.Format("restore.skipped", ("count", _restoreSkipped.ToString(CultureInfo.CurrentCulture))) : string.Empty);

    public string CareerRestoreConfirmText => _restoreProfile is not { } p || _restorePick is not { } pick ? string.Empty
        : S.Format("restore.confirm_body", ("name", ProfileName.Plain(p.Gamertag)), ("when", When(pick.Archive)))
          + (_restoreUnsent ? Environment.NewLine + S["restore.unsent_note"] : string.Empty);

    /// <summary>This PC's save is not the cloud's (unsent or parked progress): the confirmation says it is backed up and replaced.</summary>
    private bool _restoreUnsent;

    private static string When(FreshStartArchive archive) => archive.ArchivedUtc.ToLocalTime().ToString("g", CultureInfo.CurrentCulture);

    // ------------------------------------------------------------------ a restore waiting for the cloud, or an undo owed: the way out

    private Profile? _restoreWaiting;

    public bool ShowRestoreWaiting => _restoreWaiting is not null;

    /// <summary>An undo is owed and could not be paid: "Keep this career on this PC" is offered, so the player is never trapped.</summary>
    public bool CanKeepRestoredCareer => _restoreWaiting is { } profile && _install is not null && CloudSaveClient() is { } client
        && ulong.TryParse(profile.Xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var xuid)
        && RestoreUndo.Stuck(client.Saves, AzimuthSaves.SavesRoot(_install.Root), xuid);

    /// <summary>"Keep this career on this PC": the debt and the restore record go; the save follows the normal rules (nothing deleted).</summary>
    public void KeepRestoredCareer()
    {
        if (!CanKeepRestoredCareer || _restoreWaiting is not { } profile || CloudSaveClient() is not { } client || ParseXuid(profile) is not { } xuid) return;
        RestoreUndo.KeepRestored(client.Saves, xuid, Log);
        _restoreWaiting = null;
        Message = S["restore.kept_on_pc"];
        Changed(string.Empty);
    }

    /// <summary>After a Play stopped on a save step: when the chosen profile's restore is still waiting (or its undo owed), offer the way out.</summary>
    private void ObserveRestoreBlocked()
    {
        if (_install is null || CloudSaveClient() is not { } client || ChosenProfile is not { Mode: "hardcore" } profile
            || !ulong.TryParse(profile.Xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var xuid)
            || !RestoreUndo.Unsettled(client.Saves, AzimuthSaves.SavesRoot(_install.Root), xuid)) return;
        Log("restore: the Play stopped while the restored career waits for the cloud: offering to set it aside");
        _restoreWaiting = profile;
        Changed(string.Empty);
    }

    /// <summary>"Try again later": the prompt closes; the restore stays pending and the next Play sends it again.</summary>
    public void CloseRestoreWaiting()
    {
        _restoreWaiting = null;
        Changed(string.Empty);
    }

    /// <summary>
    /// "Set the restore aside and put my previous save back": <see cref="RestoreUndo.SetAsideAsync"/> (the upload's bytes kept in
    /// pending\set-aside, the save from before the restore installed again; nothing deleted). Never while the game runs.
    /// </summary>
    public async Task SetRestoreAsideAsync()
    {
        if (_restoreWaiting is not { } profile || _session is not { } session || CloudSaveClient() is not { } client || _install is null
            || ParseXuid(profile) is not { } xuid) return;
        if (Playing || System.Diagnostics.Process.GetProcessesByName("azimuth").Length > 0) { Message = S["saves.game_running"]; return; }
        var savesRoot = AzimuthSaves.SavesRoot(_install.Root);
        await Run(async () =>
        {
            var result = RestoreUndo.SetAsideResult.NotUndone;
            try
            {
                var access = await session.Tokens.AccessAsync(CancellationToken.None);
                SaveHead? head = null;
                try { head = (await client.Saves.HeadAsync(access, xuid)).Head; }
                catch (Exception e) when (e is HttpRequestException or ApiException or IOException) { Log("restore: head not read for the undo: " + e.GetType().Name); }
                result = await Task.Run(() => RestoreUndo.SetAsideAsync(client.Saves, access, savesRoot, xuid, head, DateTime.UtcNow, Log));
            }
            catch (Exception e) when (e is HttpRequestException or ApiException or IOException or UnauthorizedAccessException or OperationCanceledException
                or System.Text.DecoderFallbackException)
            {
                Log("restore: not set aside: " + e.GetType().Name);
            }
            // not undone: the prompt stays, now with "Keep this career on this PC"
            if (result is not RestoreUndo.SetAsideResult.NotUndone) _restoreWaiting = null;
            Message = S[result switch
            {
                RestoreUndo.SetAsideResult.AlreadyCommitted => "restore.already_in_cloud",
                RestoreUndo.SetAsideResult.Undone => "restore.set_aside_done",
                RestoreUndo.SetAsideResult.Busy => "saves.game_running",
                RestoreUndo.SetAsideResult.HeadUnreadable => "restore.head_failed",
                _ => "restore.undo_failed",
            }];
            Changed(string.Empty);
        });
    }

    /// <summary>Home's "Restore a career from this PC's backups": the line's head first (≥ 1), then this PC's archives.</summary>
    public Task OpenCareerRestoreAsync()
    {
        if (_install is not null && CloudSaveClient() is { } waiting && ChosenProfile is { Mode: "hardcore" } chosen
            && ulong.TryParse(chosen.Xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var chosenXuid)
            && RestoreUndo.Unsettled(waiting.Saves, AzimuthSaves.SavesRoot(_install.Root), chosenXuid))
        {
            _restoreWaiting = chosen; // a restore is pending (or its undo owed): the way out, not a second restore
            Changed(string.Empty);
            return Task.CompletedTask;
        }
        if (!CanRestoreCareer || ChosenProfile is not { Mode: "hardcore" } profile || _session is not { } session || CloudSaveClient() is not { } client
            || _install is null || ParseXuid(profile) is not { } xuid) return Task.CompletedTask;
        var savesRoot = AzimuthSaves.SavesRoot(_install.Root);
        return Run(async () =>
        {
            SaveHead? head;
            try
            {
                var access = await session.Tokens.AccessAsync(CancellationToken.None);
                (head, var failure) = await client.Saves.HeadAsync(access, xuid);
                if (failure is not null) Log("restore: the line was not read: " + failure.Code);
            }
            catch (Exception e) when (e is HttpRequestException or ApiException or IOException or OperationCanceledException)
            {
                Log("restore: the line was not read: " + e.GetType().Name);
                head = null;
            }
            if (head is null) { Message = S["restore.head_failed"]; return; }
            if (!CareerRestore.Offered(head))
            {
                Log($"restore: the line is at revision {head.Revision}: the first save must reach the cloud first");
                Message = S["restore.first_save_first"];
                return;
            }
            var listing = await Task.Run(() => FreshStartArchives.List(savesRoot, xuid));
            foreach (var skipped in listing.Skipped) Log($"restore: {skipped.Name} not offered: {skipped.Reason}");
            CareerRestoreRows.Clear();
            foreach (var archive in listing.Archives) CareerRestoreRows.Add(Describe(archive));
            if (CareerRestoreRows.Count == 0) { Message = S["restore.none"]; return; }
            Log($"restore: {CareerRestoreRows.Count} career(s) offered on revision {head.Revision}");
            _restoreProfile = profile;
            _restorePick = null;
            _restoreSkipped = listing.Skipped.Count;
            Message = null;
            Changed(string.Empty);
        });
    }

    private CareerRestoreRow Describe(FreshStartArchive archive)
    {
        var parts = new List<string>();
        if (archive.Level is { } level) parts.Add(S.Format("saves.level", ("level", level.ToString(CultureInfo.CurrentCulture))));
        if (archive.Credits is { } credits) parts.Add(S.Format("saves.credits", ("credits", credits.ToString("N0", CultureInfo.CurrentCulture))));
        return new CareerRestoreRow(archive, S.Format("restore.row_title", ("when", When(archive))), string.Join(" · ", parts));
    }

    /// <summary>A career picked: the confirmation (what happens, the backup first) replaces the list.</summary>
    public void PickCareerRestore(CareerRestoreRow row)
    {
        ArgumentNullException.ThrowIfNull(row);
        if (_restoreProfile is null) return;
        _restorePick = row;
        _restoreUnsent = _install is not null && CloudSaveClient() is { } client
            && ulong.TryParse(_restoreProfile.Xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var xuid)
            && CareerRestore.HasUnsentProgress(client.Saves, AzimuthSaves.SavesRoot(_install.Root), xuid);
        Changed(string.Empty);
    }

    /// <summary>The confirmation's "Back": the list again.</summary>
    public void BackCareerRestore()
    {
        _restorePick = null;
        Changed(string.Empty);
    }

    public void CloseCareerRestore()
    {
        _restoreProfile = null;
        _restorePick = null;
        _restoreSkipped = 0;
        _restoreWaiting = null;
        CareerRestoreRows.Clear();
        Changed(string.Empty);
    }

    /// <summary>Confirmed: the dry run, then (on a pass only) the backup, the copy and the recorded upload. Never while the game runs.</summary>
    public async Task ConfirmCareerRestoreAsync()
    {
        if (_restoreProfile is not { } profile || _restorePick is not { } pick || _session is not { } session || CloudSaveClient() is not { } client
            || _install is null || ParseXuid(profile) is not { } xuid) return;
        if (Playing || System.Diagnostics.Process.GetProcessesByName("azimuth").Length > 0) { Message = S["saves.game_running"]; return; }
        var savesRoot = AzimuthSaves.SavesRoot(_install.Root);
        await Run(async () =>
        {
            Message = S["restore.checking"];
            RestoreOutcome outcome;
            try
            {
                var access = await session.Tokens.AccessAsync(CancellationToken.None);
                outcome = await Task.Run(() => CareerRestore.RestoreAsync(client.Saves, client.Api, access, savesRoot, xuid, pick.Archive, DateTime.UtcNow, Log));
            }
            catch (Exception e) when (e is HttpRequestException or ApiException or IOException or OperationCanceledException or System.Text.DecoderFallbackException)
            {
                Log("restore: not done: " + e.GetType().Name);
                outcome = new RestoreOutcome(RestoreOutcomeKind.Failed, "NETWORK");
            }
            CloseCareerRestore();
            if (outcome.Kind == RestoreOutcomeKind.NotSupported) _restoreUnsupported = true;
            Message = outcome.Kind switch
            {
                RestoreOutcomeKind.Recorded => S.Format("restore.recorded", ("when", When(pick.Archive))),
                RestoreOutcomeKind.InstalledNotQueued => S.Format("restore.installed_not_queued", ("when", When(pick.Archive)), ("code", outcome.Code ?? "?")),
                RestoreOutcomeKind.Refused when CareerRestore.IsCeiling(outcome.Rule) => S["restore.ceiling"],
                RestoreOutcomeKind.Refused when outcome.Reasons is { Count: > 0 } reasons =>
                    S.Format("restore.refused", ("reasons", string.Join("; ", reasons.Select(SeedReasonText).Distinct(StringComparer.Ordinal)))),
                RestoreOutcomeKind.Refused => S.Format("restore.refused_code", ("code", outcome.Code is { } code ? ErrorText(code) : "?")),
                RestoreOutcomeKind.NotSupported => S["restore.unavailable"],
                RestoreOutcomeKind.NotOffered => S["restore.first_save_first"],
                _ when outcome.Code == ErrorCodes.SavesRateLimited => S["restore.retry_later"],
                _ => S.Format("restore.failed", ("code", outcome.Code ?? "?")),
            };
        });
    }
}

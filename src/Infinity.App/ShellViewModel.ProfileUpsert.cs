// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.ComponentModel;
using System.Globalization;
using System.IO;
using System.Net.Http;
using System.Runtime.CompilerServices;
using Infinity.Core.Api;
using Infinity.Core.Saves;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>One local profile in the update panel: chosen to upload or not, and how its upload went.</summary>
public sealed class UpsertRow(LocalProfile profile, string line) : INotifyPropertyChanged
{
    public LocalProfile Profile { get; private set; } = profile;
    public string Name => Profile.Name;
    public string Line { get; } = line;
    private bool _selected; private string _result = ""; private bool _done; private bool _nameTaken; private string _newName = "";
    public bool Selected { get => _selected; set { _selected = value; Changed(); SelectionChanged?.Invoke(); } }
    public string Result { get => _result; set { _result = value; Changed(); } }
    public bool Done { get => _done; set { _done = value; Changed(); Changed(nameof(CanChange)); Changed(nameof(AsksName)); } }
    public bool CanChange => !_done;

    /// <summary>OD-120: the server refused the name (taken meanwhile): the row asks for a new one; the profile and its save stay.</summary>
    public bool NameTaken { get => _nameTaken; set { _nameTaken = value; Changed(); Changed(nameof(AsksName)); } }
    public bool AsksName => _nameTaken && !_done;
    public string NewName { get => _newName; set { _newName = value; Changed(); SelectionChanged?.Invoke(); } }
    internal Action? SelectionChanged;
    internal void Update(LocalProfile profile) { Profile = profile; Changed(nameof(Name)); }

    public event PropertyChangedEventHandler? PropertyChanged;
    private void Changed([CallerMemberName] string? name = null) => PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(name));
}

/// <summary>
/// OD-118/OD-120 profiles after sign-in. Signing in registers this PC's profiles on the account by itself when they all fit (OD-120:
/// "when signing in, the profile name is pushed to the servers"), through the idempotent upsert; the "Update online profiles" panel
/// opens only for what needs the player: more than fit (they choose), a name taken meanwhile (they choose a new one) or no room left
/// (the rest stay on this PC). A registered Noob profile's newest local save is cloned into the account XUID's folder (P1b) so its
/// offline progress continues; a Hardcore one starts online from the server's seed and its local progress stays offline, never
/// uploaded. Then an account profile renamed by the OD-120 backfill gets its one-time notice with the free rename.
/// </summary>
public sealed partial class ShellViewModel
{
    private ProfileUpsertPlan? _upsertPlan;
    private bool _upsertBusy;
    public ObservableCollection<UpsertRow> UpsertRows { get; } = [];
    public ObservableCollection<string> UpsertAccountProfiles { get; } = [];

    public bool ShowProfileUpsert => _upsertPlan is not null && SignedIn;
    public string UpsertIntro => _upsertPlan is not { } plan ? "" : plan.Room == 0 ? S["upsert.no_room"]
        : S.Format(plan.MustChoose ? "upsert.choose" : "upsert.intro", ("room", plan.Room.ToString(CultureInfo.CurrentCulture)));
    public bool HasUpsertAccountProfiles => UpsertAccountProfiles.Count > 0;
    public int UpsertSelected => UpsertRows.Count(r => r.Selected && !r.Done);
    public bool CanUpload => ShowProfileUpsert && _session is not null && !_upsertBusy && UpsertSelected > 0 && UpsertSelected <= (_upsertPlan?.Room ?? 0)
        && UpsertRows.Where(r => r.Selected && r.AsksName).All(r => ProfileName.IsValid(r.NewName.Trim()));
    public string UpsertSelectionText => _upsertPlan is { } plan ? S.Format("upsert.selected", ("count", UpsertSelected.ToString(CultureInfo.CurrentCulture)),
        ("room", plan.Room.ToString(CultureInfo.CurrentCulture))) : "";

    /// <summary>
    /// After sign-in (and on every start signed in): refreshes the account cache (P3; OD-120 plain names, the local copies renamed
    /// too), registers the local profiles that all fit, and opens the panel for what is left, then the rename notice.
    /// </summary>
    internal async Task OfferProfileUpsertAsync()
    {
        if (_session is not { } session || _gate.Game is not { } game) return;
        try
        {
            var api = new ApiProfileAccount(session.Api);
            var access = await session.Tokens.AccessAsync(CancellationToken.None);
            var plan = await ProfileUpsert.PlanAsync(api, access, LocalProfiles, game, SaveFolderExists, CancellationToken.None);
            // An earlier registration the server stored but whose answer was lost: linked by this refresh, continued like any other.
            var registered = plan.Recovered.ToList();
            IReadOnlyList<ProfileUpsertResult> automatic = [];
            if (plan.Automatic.Count > 0)
            {
                // A lost answer is retried once at once: the stored upload keys make it a replay, never a second profile.
                try { automatic = await ProfileUpsert.UploadAsync(api, access, LocalProfiles, plan, plan.Automatic, CancellationToken.None); }
                catch (HttpRequestException) { automatic = await ProfileUpsert.UploadAsync(api, access, LocalProfiles, plan, plan.Automatic, CancellationToken.None); }
                registered.AddRange(automatic.Where(r => r.Uploaded).Select(r => r.Profile));
            }
            if (registered.Count > 0)
            {
                Message = S.Format("upsert.registered", ("names", string.Join(", ", registered.Select(p => p.DisplayName)))) + string.Concat(registered.Select(SaveNote));
                await session.RefreshMeAsync();
            }
            if (plan.Automatic.Count > 0) plan = await ProfileUpsert.PlanAsync(api, access, LocalProfiles, game, SaveFolderExists, CancellationToken.None);
            FillProfiles();
            RefreshChosenLocal();
            RefreshChosenTag();
            QueueRenameNotices(plan);
            if (plan.Uploadable.Count == 0) { Changed(string.Empty); return; }
            ShowUpsert(plan);
            foreach (var refused in automatic.Where(r => !r.Uploaded))
                if (UpsertRows.FirstOrDefault(r => r.Profile.Id == refused.Profile.Id) is { } row) MarkRefused(row, refused.ErrorCode!);
        }
        catch (Exception error) when (error is ApiException or HttpRequestException or IOException or OperationCanceledException or LocalProfileException)
        { Log("profiles: update panel unavailable: " + error.GetType().Name + (error is ApiException api ? " " + api.Code : "")); }
    }

    private void ShowUpsert(ProfileUpsertPlan plan)
    {
        _upsertPlan = plan;
        UpsertAccountProfiles.Clear();
        foreach (var account in plan.Account) UpsertAccountProfiles.Add(account.DisplayName + " · " + S[account.Hardcore ? "upsert.mode_hardcore" : "upsert.mode_noob"]);
        UpsertRows.Clear();
        foreach (var local in plan.Uploadable)
        {
            var row = new UpsertRow(local, S[local.Hardcore ? "upsert.rule_hardcore" : "upsert.rule_noob"]) { Selected = UpsertRows.Count < plan.Room };
            row.SelectionChanged = () => Changed(string.Empty);
            UpsertRows.Add(row);
        }
        Changed(string.Empty);
    }

    /// <summary>A refused upload's row: a taken name asks for a new one (and stays chosen); the limit and the rest say why.</summary>
    private void MarkRefused(UpsertRow row, string code)
    {
        row.NameTaken = code == ErrorCodes.AccountsNameUnavailable;
        row.Result = code switch
        {
            ErrorCodes.AccountsNameUnavailable => S["upsert.name_taken"],
            ErrorCodes.AccountsProfileLimit => S["upsert.limit_kept"],
            _ => ProfileErrorText(code),
        };
    }

    public void CloseProfileUpsert()
    {
        if (_upsertBusy) return;
        _upsertPlan = null; UpsertRows.Clear(); UpsertAccountProfiles.Clear();
        FillProfiles();
        RefreshChosenLocal(); Changed(string.Empty);
    }

    public async Task UploadProfilesAsync()
    {
        if (!CanUpload || _session is not { } session || _upsertPlan is not { } plan) return;
        _upsertBusy = true; Changed(string.Empty);
        try
        {
            // OD-120: a name taken meanwhile is renamed on this PC first (a new upload key: the server replays a key's first outcome).
            foreach (var row in UpsertRows.Where(r => r.Selected && r.AsksName).ToArray())
            {
                try { row.Update(LocalProfiles.Edit(row.Profile.Id, row.NewName.Trim(), row.Profile.Mode)); row.NameTaken = false; row.Result = ""; }
                catch (LocalProfileException refused) { row.Result = ProfileErrorText(refused.Code); row.Selected = false; }
            }
            var chosen = UpsertRows.Where(r => r.Selected && !r.Done).Select(r => r.Profile.Id).ToArray();
            if (chosen.Length == 0) return;
            var access = await session.Tokens.AccessAsync(CancellationToken.None);
            var results = await ProfileUpsert.UploadAsync(new ApiProfileAccount(session.Api), access, LocalProfiles, plan, chosen, CancellationToken.None);
            foreach (var result in results)
            {
                if (UpsertRows.FirstOrDefault(r => r.Profile.Id == result.Profile.Id) is not { } row) continue;
                row.Update(result.Profile);
                if (!result.Uploaded) { MarkRefused(row, result.ErrorCode!); continue; }
                row.Done = true; row.Selected = false;
                row.Result = S.Format("upsert.uploaded", ("gamertag", result.Profile.DisplayName)) + SaveNote(result.Profile);
            }
            await session.RefreshMeAsync();
            FillProfiles();
            RefreshChosenLocal();
            Message = results.All(r => r.Uploaded) ? S["upsert.done"] : S["upsert.partly"];
        }
        catch (Exception error) when (error is ApiException or HttpRequestException or OperationCanceledException or LocalProfileException or IOException)
        {
            Log("profiles: upload failed: " + error.GetType().Name + (error is ApiException api ? " " + api.Code : ""));
            Message = error is LocalProfileException local ? ProfileErrorText(local.Code) : S["upsert.retry"]; // the stored keys make the retry safe
        }
        finally { _upsertBusy = false; Changed(string.Empty); }
    }

    /// <summary>
    /// What a registered profile's saves do: Noob continues its offline save online (the clone below); Hardcore starts online from
    /// the server's seed (OD-60) and its local progress stays its offline progress, never uploaded or cloned.
    /// </summary>
    private string SaveNote(LocalProfile profile) => profile.Hardcore ? " " + S["upsert.rule_hardcore"] : ContinueNoobSave(profile);

    /// <summary>P1b through <see cref="ProfileUpsert.ContinueOfflineSave"/>: an uploaded Noob profile's offline save continues online.</summary>
    private string ContinueNoobSave(LocalProfile profile)
    {
        if (_install is not { } install) return "";
        var outcome = ProfileUpsert.ContinueOfflineSave(AzimuthSaves.SavesRoot(install.Root), _stateDirectory, profile, DateTime.UtcNow);
        Log("profiles: offline save of " + profile.Id + ": " + outcome);
        return outcome switch
        {
            ProfileUpsert.SaveContinuation.Continued => " " + S["upsert.save_continues"],
            ProfileUpsert.SaveContinuation.Kept => " " + S["upsert.save_kept"],
            _ => "",
        };
    }
    /// <summary>The chosen profile's shown name follows a server rename (the selection itself is keyed by its row id, OD-120).</summary>
    private void RefreshChosenTag()
    {
        if (_gate.ProfileKey is not { } key || Profiles.FirstOrDefault(p => p.ProfileId == key) is not { } row) return;
        var name = ProfileName.Plain(row.Gamertag);
        if (name != _gate.ProfileTag) Gate = _gate.ChooseProfile(name, key);
    }

    // ---- OD-120 free rename after a clash rename (ACCOUNTS.md §5 "Plain names and renames") ----

    private readonly Queue<LocalProfile> _renameQueue = new();
    private readonly HashSet<string> _renameLater = new(StringComparer.Ordinal);
    private readonly Dictionary<(string ProfileId, string Name), string> _renameKeys = [];
    private LocalProfile? _renaming;
    private string _renameName = "";
    private bool _renameBusy;

    /// <summary>The one-time notice: shown once the update panel is closed, for one renamed profile at a time.</summary>
    public bool ShowRenameNotice => _renaming is not null && SignedIn && !ShowProfileUpsert && !ShowPickSave;
    public string RenameNoticeText => _renaming?.Cloud?.RenameNotice is { } notice
        ? S.Format("rename.notice", ("previous", notice.PreviousName), ("name", _renaming.DisplayName)) : "";
    public string RenameKeepLabel => _renaming is null ? "" : S.Format("rename.keep", ("name", _renaming.DisplayName));
    public string RenameName { get => _renameName; set { _renameName = value; Changed(); Changed(nameof(CanRename)); } }
    public bool CanRename => ShowRenameNotice && !_renameBusy && ProfileName.IsValid(_renameName.Trim());
    public bool CanKeepName => ShowRenameNotice && !_renameBusy;

    private void QueueRenameNotices(ProfileUpsertPlan plan)
    {
        if (_renameBusy) return; // the rename in flight finishes first; the next refresh queues the rest
        var shown = _renaming?.Cloud?.ProfileId;
        _renameQueue.Clear();
        foreach (var profile in plan.RenameNotices.Where(p => !_renameLater.Contains(p.Cloud!.ProfileId) && p.Cloud!.ProfileId != shown)) _renameQueue.Enqueue(profile);
        // The notice on screen stays (with what the player typed) while the server still lists it; otherwise the next one shows.
        if (shown is not null && plan.RenameNotices.FirstOrDefault(p => p.Cloud!.ProfileId == shown) is { } current) { _renaming = current; Changed(string.Empty); }
        else NextRenameNotice();
    }

    private void NextRenameNotice()
    {
        _renaming = _renameQueue.Count > 0 ? _renameQueue.Dequeue() : null;
        _renameName = string.Empty;
        Changed(string.Empty);
    }

    /// <summary>"Later": the notice comes back at the next start; the free rename stays unused.</summary>
    public void RenameLater()
    {
        if (_renameBusy || _renaming?.Cloud is not { } cloud) return;
        _renameLater.Add(cloud.ProfileId);
        NextRenameNotice();
    }

    /// <summary>Keeps the server's new name (sending it as the rename clears the notice; no other rename is offered afterwards).</summary>
    public Task KeepRenamedNameAsync() => _renaming is { } profile ? RenameToAsync(profile, profile.DisplayName) : Task.CompletedTask;

    public Task RenameProfileAsync() => _renaming is { } profile && CanRename ? RenameToAsync(profile, _renameName.Trim()) : Task.CompletedTask;

    private async Task RenameToAsync(LocalProfile profile, string name)
    {
        if (_renameBusy || _session is not { } session || profile.Cloud is not { } cloud) return;
        _renameBusy = true; Changed(string.Empty);
        try
        {
            var keyOf = (cloud.ProfileId, name); // the exact body sent: another spelling is another request
            if (!_renameKeys.TryGetValue(keyOf, out var key)) _renameKeys[keyOf] = key = Guid.CreateVersion7().ToString();
            var access = await session.Tokens.AccessAsync(CancellationToken.None);
            var result = await ProfileUpsert.RenameAsync(new ApiProfileAccount(session.Api), access, LocalProfiles, profile, name, key, CancellationToken.None);
            if (result.ErrorCode is { } code)
            {
                _renameKeys.Remove(keyOf); // a refusal is final for that key: the next attempt is a new request
                Message = code switch
                {
                    ErrorCodes.AccountsNameUnavailable => S["offline.error.name_unavailable"],
                    ErrorCodes.AccountsInvalidName => S["profiles.name_rule"],
                    ErrorCodes.AccountsRenameNotAllowed => S["rename.not_allowed"],
                    ErrorCodes.SessionsProfileInUse => S["rename.in_use"],
                    _ => S["rename.retry"],
                };
                if (code == ErrorCodes.AccountsRenameNotAllowed) { await session.RefreshMeAsync(); FillProfiles(); NextRenameNotice(); }
                return;
            }
            _renameKeys.Remove(keyOf);
            var renamed = result.Profile.DisplayName;
            Message = S.Format(string.Equals(renamed, profile.DisplayName, StringComparison.OrdinalIgnoreCase) ? "rename.kept" : "rename.done", ("name", renamed));
            await session.RefreshMeAsync();
            FillProfiles();
            RefreshChosenLocal();
            RefreshChosenTag();
            NextRenameNotice();
        }
        catch (Exception error) when (error is ApiException or HttpRequestException or OperationCanceledException or IOException)
        {
            Log("profiles: rename failed: " + error.GetType().Name + (error is ApiException api ? " " + api.Code : ""));
            Message = S["rename.retry"]; // the kept key makes the retry a replay
        }
        finally { _renameBusy = false; Changed(string.Empty); }
    }
}

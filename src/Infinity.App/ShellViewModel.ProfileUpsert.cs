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
    private bool _selected; private string _result = ""; private bool _done;
    public bool Selected { get => _selected; set { _selected = value; Changed(); SelectionChanged?.Invoke(); } }
    public string Result { get => _result; set { _result = value; Changed(); } }
    public bool Done { get => _done; set { _done = value; Changed(); Changed(nameof(CanChange)); } }
    public bool CanChange => !_done;
    internal Action? SelectionChanged;
    internal void Update(LocalProfile profile) => Profile = profile;

    public event PropertyChangedEventHandler? PropertyChanged;
    private void Changed([CallerMemberName] string? name = null) => PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(name));
}

/// <summary>
/// OD-118 "Update online profiles": shown after sign-in when this PC has profiles that are not on the account. The account's
/// profiles (other PCs' included) are listed; the local ones are uploaded through the idempotent upsert, the player choosing which
/// when they do not all fit (the rest stay offline-only). An uploaded Noob profile's newest local save is cloned into the account
/// XUID's folder (P1b) so its offline progress continues; Hardcore starts from the server's seed and its offline progress stays.
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
    public bool CanUpload => ShowProfileUpsert && _session is not null && !_upsertBusy && UpsertSelected > 0 && UpsertSelected <= (_upsertPlan?.Room ?? 0);
    public string UpsertSelectionText => _upsertPlan is { } plan ? S.Format("upsert.selected", ("count", UpsertSelected.ToString(CultureInfo.CurrentCulture)),
        ("room", plan.Room.ToString(CultureInfo.CurrentCulture))) : "";

    /// <summary>After sign-in: refreshes the account cache (P3) and opens the panel when a local profile could go online.</summary>
    internal async Task OfferProfileUpsertAsync()
    {
        if (_session is not { } session || _gate.Game is not { } game) return;
        try
        {
            var access = await session.Tokens.AccessAsync(CancellationToken.None);
            var plan = await ProfileUpsert.PlanAsync(new ApiProfileAccount(session.Api), access, LocalProfiles, game, SaveFolderExists, CancellationToken.None);
            FillProfiles();
            if (plan.Uploadable.Count == 0) return;
            ShowUpsert(plan);
        }
        catch (Exception error) when (error is ApiException or HttpRequestException or IOException or OperationCanceledException)
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

    public void CloseProfileUpsert()
    {
        if (_upsertBusy) return;
        _upsertPlan = null; UpsertRows.Clear(); UpsertAccountProfiles.Clear();
        FillProfiles(); Changed(string.Empty);
    }

    public async Task UploadProfilesAsync()
    {
        if (!CanUpload || _session is not { } session || _upsertPlan is not { } plan) return;
        var chosen = UpsertRows.Where(r => r.Selected && !r.Done).Select(r => r.Profile.Id).ToArray();
        _upsertBusy = true; Changed(string.Empty);
        try
        {
            var access = await session.Tokens.AccessAsync(CancellationToken.None);
            var results = await ProfileUpsert.UploadAsync(new ApiProfileAccount(session.Api), access, LocalProfiles, plan, chosen, CancellationToken.None);
            foreach (var result in results)
            {
                if (UpsertRows.FirstOrDefault(r => r.Profile.Id == result.Profile.Id) is not { } row) continue;
                row.Update(result.Profile);
                if (!result.Uploaded) { row.Result = ProfileErrorText(result.ErrorCode!); continue; }
                row.Done = true; row.Selected = false;
                row.Result = S.Format("upsert.uploaded", ("gamertag", result.Profile.DisplayName)) + ContinueNoobSave(result.Profile);
            }
            await session.RefreshMeAsync();
            FillProfiles();
            Message = results.All(r => r.Uploaded) ? S["upsert.done"] : S["upsert.partly"];
        }
        catch (Exception error) when (error is ApiException or HttpRequestException or OperationCanceledException or LocalProfileException)
        {
            Log("profiles: upload failed: " + error.GetType().Name + (error is ApiException api ? " " + api.Code : ""));
            Message = error is LocalProfileException local ? ProfileErrorText(local.Code) : S["upsert.retry"]; // the stored keys make the retry safe
        }
        finally { _upsertBusy = false; Changed(string.Empty); }
    }

    /// <summary>
    /// P1b: an uploaded Noob profile continues its offline save online. Its newest sealed local revision (its own offline XUID's
    /// folder) is cloned into the account XUID's folder with the OD-41 clone (backups of what it shadows); none, or Hardcore: nothing.
    /// </summary>
    private string ContinueNoobSave(LocalProfile profile)
    {
        if (profile.Hardcore || _install is not { } install || profile.Cloud is not { } cloud
            || !ulong.TryParse(cloud.Xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var xuid)) return "";
        try
        {
            var root = AzimuthSaves.SavesRoot(install.Root);
            var source = AzimuthSaves.List(root).FirstOrDefault(s => s.Xuid == profile.OfflineXuid);
            if (source is null) return "";
            var result = AzimuthSaves.Clone(root, source, xuid, DateTime.UtcNow);
            new SaveLinks(_stateDirectory).Set(cloud.Xuid, new SaveLink(source.Folder, source.Revision.Number, result.Number, DateTime.UtcNow));
            Log("profiles: offline save of " + profile.Id + " continues in " + AzimuthSaves.FolderFor(xuid));
            return " " + S["upsert.save_continues"];
        }
        catch (Exception error) when (error is SaveException or IOException or UnauthorizedAccessException)
        {
            Log("profiles: offline save not continued: " + error.GetType().Name);
            return " " + S["upsert.save_kept"];
        }
    }
}

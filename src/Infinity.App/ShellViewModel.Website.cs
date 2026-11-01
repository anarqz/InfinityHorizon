// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.IO;
using System.Net.Http;
using System.Text.Json;
using Infinity.Core.Api;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>
/// CR-L47 (CR-S83, OD-168 follow-up DB-Q4): the account page's Website card. Each live account profile has its "Show me on the website"
/// switch (the play dashboard), read from <c>Profile.websiteListed</c> on <c>/me</c>: a server before CR-S83 sends none and the card is
/// hidden, a <c>deletion_pending</c> profile has none. Signed in only (OD-118). Sent at once, one request per switch at a time, and put
/// back to the last server value when the server refuses or the answer is lost; the answer replaces the profile in the session's /me.
/// </summary>
public sealed partial class ShellViewModel
{
    private string? _websiteStatus;

    /// <summary>Each eligible profile's switch, from the session's last /me.</summary>
    public ObservableCollection<WebsiteSwitch> WebsiteSwitches { get; } = [];

    public bool ShowWebsite => SignedIn && !OfflineMode && WebsiteSwitches.Count > 0;

    public string? WebsiteStatus => _websiteStatus;

    public bool HasWebsiteStatus => !string.IsNullOrEmpty(_websiteStatus);

    /// <summary>The switches again from /me; a switch whose request is in flight is kept as it is (its answer lands on it).</summary>
    private void FillWebsiteSwitches()
    {
        var profiles = !OfflineMode && _session?.Me is { } me ? WebsiteSwitch.Eligible(me.Profiles).ToList() : [];
        var busy = WebsiteSwitches.Where(s => s.Busy).ToDictionary(s => s.Xuid, StringComparer.Ordinal);
        WebsiteSwitches.Clear();
        foreach (var profile in profiles) WebsiteSwitches.Add(busy.TryGetValue(profile.Xuid, out var kept) ? kept : new WebsiteSwitch(profile));
        ChangedWebsite();
    }

    /// <summary>A profile's switch was clicked: shown at once, sent, and put back on a refusal or a lost answer.</summary>
    public async Task SetWebsiteListedAsync(WebsiteSwitch row, bool wanted)
    {
        ArgumentNullException.ThrowIfNull(row);
        if (!SignedIn || OfflineMode || _session is not { } session) return;
        try
        {
            await row.SetAsync(wanted, (value, ct) => session.SetWebsiteListedAsync(row.Xuid, value, ct));
            SetWebsiteStatus(null);
        }
        catch (ApiException e)
        {
            Log("website: listing refused " + e.Code);
            SetWebsiteStatus(WebsiteErrors.TextKey(e) is { } key ? S[key] : ErrorText(e.Code));
            if (WebsiteErrors.RereadsProfiles(e) && _session == session) await RereadWebsiteProfilesAsync(session);
        }
        catch (Exception e) when (e is HttpRequestException or IOException or OperationCanceledException or TimeoutException or JsonException)
        {
            Log("website: listing not answered (" + e.GetType().Name + ")");
            SetWebsiteStatus(S["native.unreachable"]);
        }
    }

    /// <summary>The profile is gone or pending deletion: /me again, so its switch leaves the card.</summary>
    private async Task RereadWebsiteProfilesAsync(Infinity.Core.Auth.AccountSession session)
    {
        try
        {
            await session.RefreshMeAsync();
            if (_session == session) FillProfiles();
        }
        catch (Exception e) when (e is ApiException or HttpRequestException or IOException or OperationCanceledException or TimeoutException or JsonException)
        {
            Log("website: profiles not read again (" + e.GetType().Name + ")");
        }
    }

    /// <summary>Signed out: no switch of this account stays on screen.</summary>
    private void ClearWebsite()
    {
        _websiteStatus = null;
        WebsiteSwitches.Clear();
        ChangedWebsite();
    }

    private void SetWebsiteStatus(string? text)
    {
        _websiteStatus = text;
        Changed(nameof(WebsiteStatus));
        Changed(nameof(HasWebsiteStatus));
    }

    private void ChangedWebsite()
    {
        foreach (var name in new[] { nameof(ShowWebsite), nameof(WebsiteStatus), nameof(HasWebsiteStatus) }) Changed(name);
    }
}

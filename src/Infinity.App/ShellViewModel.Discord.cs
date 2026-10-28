// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.Diagnostics;
using System.Globalization;
using System.IO;
using System.Net.Http;
using System.Text.Json;
using System.Windows.Media;
using System.Windows.Media.Imaging;
using Infinity.Core.Api;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>
/// CR-S63 (owner-decisions 2026-10-27 "Players link their Discord account"; CORE_API.md §5a.2): the account page's Discord card. Signed
/// in only (OD-118: nothing is sent before SIGN IN, and never on the offline path). "Link Discord" (shown when the server offers it)
/// asks core for a one-time code, opens the bot's page in the default browser and waits for the link (every 3 s, at most 10 minutes,
/// a Cancel and a Check now). Linked, the handle and the Discord avatar (the account's avatar) show on the card and on every profile
/// of the account; "Unlink" clears it. Each live profile has its "Announce me on Discord" switch (on by default), sent at once and put
/// back when the server refuses. The link is the account's: a deleted profile keeps it.
/// </summary>
public sealed partial class ShellViewModel
{
    private const int AvatarMaxBytes = 1 << 20;
    private DiscordSettings? _discord;
    private CancellationTokenSource? _discordPoll;
    private bool _discordBusy;
    private string? _discordStatus;
    private ImageSource? _discordAvatar;
    private string? _discordAvatarUrl;
    private int _discordEpoch;
    private HttpClient? _discordAvatarHttp;

    /// <summary>Harness hooks: the browser, the poll's wait and clock, and the avatar download (null: the real ones).</summary>
    internal Action<Uri>? OpenBrowserForTests { get; set; }
    internal Func<TimeSpan, CancellationToken, Task>? DiscordDelayForTests { get; set; }
    internal TimeProvider DiscordClock { get; set; } = TimeProvider.System;
    internal Func<Uri, CancellationToken, Task<byte[]?>>? DiscordAvatarFetchForTests { get; set; }

    /// <summary>Each live profile's switch, from the last <c>GET /core/v1/me/discord</c>.</summary>
    public ObservableCollection<AnnounceSwitch> AnnounceSwitches { get; } = [];

    private bool DiscordSignedIn
    {
        get
        {
            var signedIn = SignedIn && !OfflineMode && _session is not null;
            InspectDiscordSignedIn(ref signedIn);
            return signedIn;
        }
    }

    /// <summary>The harness's preview screens have no session.</summary>
    partial void InspectDiscordSignedIn(ref bool signedIn);

    /// <summary>The card shows once the server answered (an older server without the routes shows nothing).</summary>
    public bool ShowDiscord => DiscordSignedIn && _discord is not null;

    public bool HasDiscordLink => ShowDiscord && _discord!.Link is not null;

    /// <summary>"Link Discord": hidden when the server offers no linking (<c>available</c> false) or already linked.</summary>
    public bool ShowLinkDiscord => ShowDiscord && _discord!.Available && _discord.Link is null && !DiscordWaiting;

    public bool DiscordWaiting => _discordPoll is not null;

    public bool CanUseDiscord => DiscordSignedIn && !_discordBusy;

    public bool HasAnnounceSwitches => ShowDiscord && AnnounceSwitches.Count > 0;

    public string DiscordHandle => _discord?.Link is { } link ? DiscordLinks.Handle(link) : string.Empty;

    public string DiscordInitial => _discord?.Link is { } link ? DiscordLinks.Initial(link) : string.Empty;

    public ImageSource? DiscordAvatar => HasDiscordLink ? _discordAvatar : null;

    public bool HasDiscordAvatar => DiscordAvatar is not null;

    public string DiscordLinkedText => _discord?.Link is { } link
        ? S.Format("discord.linked_since", ("date", LocalTime(link.LinkedAt))) : string.Empty;

    public string? DiscordStatus => _discordStatus;

    public bool HasDiscordStatus => !string.IsNullOrEmpty(_discordStatus);

    /// <summary><c>GET /core/v1/me/discord</c>; quiet: a refusal only hides the card (sign-in and page opening).</summary>
    public async Task RefreshDiscordAsync(bool quiet = true)
    {
        if (!DiscordSignedIn || _session is not { } session) return;
        var epoch = _discordEpoch;
        try
        {
            var settings = await session.DiscordSettingsAsync();
            if (epoch == _discordEpoch && _session == session) ApplyDiscord(settings);
        }
        catch (ApiException e)
        {
            Log("discord: settings not read (" + e.Code + ")");
            if (!quiet && epoch == _discordEpoch) SetDiscordStatus(DiscordErrorText(e));
        }
        catch (Exception e) when (e is HttpRequestException or IOException or OperationCanceledException or TimeoutException or JsonException)
        {
            Log("discord: settings not read (" + e.GetType().Name + ")");
            if (!quiet && epoch == _discordEpoch) SetDiscordStatus(S["native.unreachable"]);
        }
    }

    /// <summary>"Link Discord": a new code, the bot's page in the browser, then the wait for the link.</summary>
    public async Task LinkDiscordAsync()
    {
        if (!CanUseDiscord || DiscordWaiting || _session is not { } session) return;
        var epoch = _discordEpoch;
        DiscordLinkCode code;
        _discordBusy = true; ChangedDiscord();
        try
        {
            code = await session.CreateDiscordLinkAsync();
        }
        catch (ApiException e)
        {
            Log("discord: link refused " + e.Code);
            SetDiscordStatus(DiscordErrorText(e));
            return;
        }
        catch (Exception e) when (e is HttpRequestException or IOException or OperationCanceledException or TimeoutException or JsonException)
        {
            Log("discord: link not answered (" + e.GetType().Name + ")");
            SetDiscordStatus(S["native.unreachable"]);
            return;
        }
        finally { _discordBusy = false; ChangedDiscord(); }
        if (epoch != _discordEpoch || _session != session) return;
        if (DiscordLinks.LinkPage(code.Url) is not { } page) { Log("discord: link page refused (not https)"); SetDiscordStatus(S["discord.bad_page"]); return; }
        if (!OpenBrowser(page)) { SetDiscordStatus(S["discord.browser_failed"]); return; }
        await WaitForDiscordLinkAsync(session, epoch);
    }

    private async Task WaitForDiscordLinkAsync(Infinity.Core.Auth.AccountSession session, int epoch)
    {
        using var poll = new CancellationTokenSource();
        _discordPoll = poll;
        SetDiscordStatus(S["discord.waiting"]);
        ChangedDiscord();
        try
        {
            var result = await DiscordLinkPoll.RunAsync(ct => session.DiscordSettingsAsync(ct), DiscordClock, DiscordDelayForTests, poll.Token,
                // only the link is applied: the switches are not rebuilt every 3 s under the player's pointer
                seen: settings => { if (settings.Link is not null && epoch == _discordEpoch && !poll.IsCancellationRequested) ApplyDiscord(settings); });
            if (epoch != _discordEpoch) return;
            Log("discord: link wait ended " + result.End);
            SetDiscordStatus(result.End switch
            {
                DiscordPollEnd.Linked => S["discord.linked_toast"],
                DiscordPollEnd.TimedOut => S["discord.timed_out"],
                _ => S["discord.cancelled"],
            });
        }
        catch (ApiException e)
        {
            Log("discord: link wait refused " + e.Code);
            if (epoch == _discordEpoch) SetDiscordStatus(DiscordErrorText(e));
        }
        finally
        {
            if (_discordPoll == poll) _discordPoll = null;
            ChangedDiscord();
        }
    }

    /// <summary>The wait's Cancel (also leaving the page and signing out).</summary>
    public void CancelDiscordLink() => _discordPoll?.Cancel();

    /// <summary>"Check now" while waiting: one read at once (the wait goes on until the link shows).</summary>
    public Task CheckDiscordNowAsync() => RefreshDiscordAsync(quiet: false);

    /// <summary>"Unlink": <c>DELETE /core/v1/me/discord/link</c>, then the settings again.</summary>
    public async Task UnlinkDiscordAsync()
    {
        if (!CanUseDiscord || _session is not { } session) return;
        _discordBusy = true; ChangedDiscord();
        try
        {
            await session.UnlinkDiscordAsync();
            Log("discord: unlinked");
            if (_discord is { } known) ApplyDiscord(known with { Link = null }); // shown at once; the read below confirms
            SetDiscordStatus(S["discord.unlinked_toast"]);
            await RefreshDiscordAsync();
        }
        catch (ApiException e)
        {
            Log("discord: unlink refused " + e.Code);
            SetDiscordStatus(DiscordErrorText(e));
        }
        catch (Exception e) when (e is HttpRequestException or IOException or OperationCanceledException or TimeoutException)
        {
            Log("discord: unlink not answered (" + e.GetType().Name + ")");
            SetDiscordStatus(S["native.unreachable"]);
        }
        finally { _discordBusy = false; ChangedDiscord(); }
    }

    /// <summary>A profile's switch was clicked: shown at once, sent, and put back on a refusal or a lost answer.</summary>
    public async Task SetAnnounceAsync(AnnounceSwitch row, bool wanted)
    {
        ArgumentNullException.ThrowIfNull(row);
        if (!DiscordSignedIn || _session is not { } session) return;
        try
        {
            await row.SetAsync(wanted, (value, ct) => session.SetAnnounceAsync(row.Xuid, value, ct));
            SetDiscordStatus(null);
        }
        catch (ApiException e)
        {
            Log("discord: announce refused " + e.Code);
            SetDiscordStatus(DiscordErrorText(e));
            if (e.Code == ErrorCodes.AccountsNotFound) await RefreshDiscordAsync(); // the profile is gone: the list again
        }
        catch (Exception e) when (e is HttpRequestException or IOException or OperationCanceledException or TimeoutException or JsonException)
        {
            Log("discord: announce not answered (" + e.GetType().Name + ")");
            SetDiscordStatus(S["native.unreachable"]);
        }
    }

    /// <summary>A Discord route's refusal in the player's words (429 with its wait).</summary>
    internal string DiscordErrorText(ApiException error) => DiscordErrors.TextKey(error) is { } key
        ? S.Format(key, ("seconds", (DiscordErrors.RetrySeconds(error) ?? 0).ToString(CultureInfo.CurrentCulture)))
        : ErrorText(error.Code);

    internal void ApplyDiscord(DiscordSettings settings)
    {
        ArgumentNullException.ThrowIfNull(settings);
        _discord = settings;
        AnnounceSwitches.Clear();
        foreach (var profile in settings.Profiles) AnnounceSwitches.Add(new AnnounceSwitch(profile));
        var avatar = settings.Link?.AvatarUrl;
        if (avatar != _discordAvatarUrl)
        {
            _discordAvatarUrl = avatar;
            _discordAvatar = null;
            if (DiscordLinks.Avatar(avatar) is { } url) _ = LoadDiscordAvatarAsync(url, _discordEpoch);
        }
        ChangedDiscord();
    }

    private async Task LoadDiscordAvatarAsync(Uri url, int epoch)
    {
        byte[]? bytes;
        try
        {
            using var bound = new CancellationTokenSource(TimeSpan.FromSeconds(15));
            bytes = DiscordAvatarFetchForTests is { } fetch ? await fetch(url, bound.Token) : await FetchAvatarAsync(url, bound.Token);
        }
        catch (Exception e) when (e is HttpRequestException or IOException or OperationCanceledException or TimeoutException or InvalidDataException)
        {
            Log("discord: avatar not loaded (" + e.GetType().Name + ")");
            return;
        }
        if (bytes is not { Length: > 0 } || await Task.Run(() => Avatar(bytes)) is not { } image) return;
        if (epoch != _discordEpoch || _discordAvatarUrl != url.OriginalString) return; // signed out or changed meanwhile
        _discordAvatar = image;
        ChangedDiscord();
    }

    /// <summary>Discord's CDN only (checked by the caller), no redirects, at most 1 MiB.</summary>
    private async Task<byte[]?> FetchAvatarAsync(Uri url, CancellationToken cancellation)
    {
        _discordAvatarHttp ??= new HttpClient(new HttpClientHandler { AllowAutoRedirect = false }) { Timeout = TimeSpan.FromSeconds(15) };
        using var response = await _discordAvatarHttp.GetAsync(url, HttpCompletionOption.ResponseHeadersRead, cancellation);
        if (!response.IsSuccessStatusCode || response.Content.Headers.ContentLength > AvatarMaxBytes) return null;
        await using var stream = await response.Content.ReadAsStreamAsync(cancellation);
        using var copy = new MemoryStream();
        var buffer = new byte[16 * 1024];
        for (int read; (read = await stream.ReadAsync(buffer, cancellation)) > 0;)
        {
            if (copy.Length + read > AvatarMaxBytes) throw new InvalidDataException("avatar too large");
            copy.Write(buffer, 0, read);
        }
        return copy.ToArray();
    }

    private static BitmapImage? Avatar(byte[] bytes)
    {
        try
        {
            var bitmap = new BitmapImage();
            using (var stream = new MemoryStream(bytes)) { bitmap.BeginInit(); bitmap.CacheOption = BitmapCacheOption.OnLoad; bitmap.DecodePixelWidth = 96; bitmap.StreamSource = stream; bitmap.EndInit(); }
            bitmap.Freeze();
            return bitmap;
        }
        catch (Exception error) when (error is IOException or NotSupportedException or InvalidOperationException or ArgumentException or FileFormatException) { return null; }
    }

    private bool OpenBrowser(Uri page)
    {
        if (OpenBrowserForTests is { } open) { open(page); return true; }
        try
        {
            Process.Start(new ProcessStartInfo(page.AbsoluteUri) { UseShellExecute = true })?.Dispose();
            return true;
        }
        catch (Exception e) when (e is System.ComponentModel.Win32Exception or InvalidOperationException)
        {
            Log("discord: browser not opened (" + e.GetType().Name + ")");
            return false;
        }
    }

    /// <summary>Leaving the account page ends the wait (the link, once made, still shows on the next read).</summary>
    private void StopDiscordWait() => _discordPoll?.Cancel();

    /// <summary>Signed out: the wait ends and nothing of this account's Discord stays on screen; late answers are dropped.</summary>
    private void ClearDiscord()
    {
        _discordEpoch++;
        _discordPoll?.Cancel();
        _discord = null;
        _discordAvatar = null;
        _discordAvatarUrl = null;
        _discordStatus = null;
        AnnounceSwitches.Clear();
        ChangedDiscord();
    }

    private void SetDiscordStatus(string? text)
    {
        _discordStatus = text;
        Changed(nameof(DiscordStatus));
        Changed(nameof(HasDiscordStatus));
    }

    private void ChangedDiscord()
    {
        foreach (var name in new[] { nameof(ShowDiscord), nameof(HasDiscordLink), nameof(ShowLinkDiscord), nameof(DiscordWaiting), nameof(CanUseDiscord),
            nameof(HasAnnounceSwitches), nameof(DiscordHandle), nameof(DiscordInitial), nameof(DiscordAvatar), nameof(HasDiscordAvatar), nameof(DiscordLinkedText) })
            Changed(name);
        OverlayAvatarChanged(); // CR-L40: the in-game guide shows the same avatar
    }
}

// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.ComponentModel;
using System.Net;
using System.Runtime.CompilerServices;
using System.Text;
using Infinity.Core.Api;

namespace Infinity.Core.Shell;

/// <summary>How a wait for the Discord link ended.</summary>
public enum DiscordPollEnd
{
    /// <summary><c>GET /core/v1/me/discord</c> answered with <c>link</c> set.</summary>
    Linked,
    /// <summary>Ten minutes passed (the link code's life) without a link.</summary>
    TimedOut,
    /// <summary>The player cancelled, left the account page or signed out.</summary>
    Cancelled,
}

public sealed record DiscordPollResult(DiscordPollEnd End, DiscordSettings? Last);

/// <summary>
/// CR-S63 (CORE_API.md §5a.2): after the bot's page is opened, <c>GET /core/v1/me/discord</c> every 3 s for at most 10 minutes until
/// <c>link</c> is set. A lost answer, a 5xx or a 429 (after its <c>Retry-After</c>) keeps waiting; any other refusal ends the wait
/// (thrown to the caller).
/// </summary>
public static class DiscordLinkPoll
{
    public static TimeSpan Interval { get; } = TimeSpan.FromSeconds(3);

    public static TimeSpan Window { get; } = TimeSpan.FromMinutes(10);

    /// <param name="read">One <c>GET /core/v1/me/discord</c>.</param>
    /// <param name="clock">The time the window is measured on.</param>
    /// <param name="delay">The wait between reads (tests pass one that moves a fake clock); null: <see cref="Task.Delay(TimeSpan, TimeProvider, CancellationToken)"/>.</param>
    /// <param name="seen">Each settings read (the caller refreshes its view).</param>
    public static async Task<DiscordPollResult> RunAsync(Func<CancellationToken, Task<DiscordSettings>> read, TimeProvider clock,
        Func<TimeSpan, CancellationToken, Task>? delay, CancellationToken cancellation, Action<DiscordSettings>? seen = null)
    {
        ArgumentNullException.ThrowIfNull(read);
        ArgumentNullException.ThrowIfNull(clock);
        delay ??= (wait, token) => Task.Delay(wait, clock, token);
        var start = clock.GetUtcNow();
        DiscordSettings? last = null;
        var next = Interval;
        try
        {
            while (true)
            {
                var left = Window - (clock.GetUtcNow() - start);
                if (left <= TimeSpan.Zero) return new(DiscordPollEnd.TimedOut, last);
                await delay(next < left ? next : left, cancellation).ConfigureAwait(false);
                cancellation.ThrowIfCancellationRequested();
                next = Interval;
                try
                {
                    last = await read(cancellation).ConfigureAwait(false);
                    seen?.Invoke(last);
                    if (last.Link is not null) return new(DiscordPollEnd.Linked, last);
                }
                catch (ApiException e) when (e.Status == HttpStatusCode.TooManyRequests || (int)e.Status >= 500)
                {
                    if (e.RetryAfter is { } after && after > next) next = after;
                }
                catch (Exception e) when (e is HttpRequestException or IOException or TimeoutException
                    || e is OperationCanceledException && !cancellation.IsCancellationRequested)
                {
                    // a lost answer (or the client's own timeout): the next read tries again
                }
            }
        }
        catch (OperationCanceledException) when (cancellation.IsCancellationRequested)
        {
            return new(DiscordPollEnd.Cancelled, last);
        }
    }
}

/// <summary>
/// One profile's "Announce me on Discord" switch: the change shows at once (optimistic), the server's answer is kept, and any failure
/// puts the previous value back before the error reaches the caller.
/// </summary>
public sealed class AnnounceSwitch(ProfileAnnounce profile) : INotifyPropertyChanged
{
    private bool _announce = profile?.Announce ?? throw new ArgumentNullException(nameof(profile));
    private bool _busy;

    public event PropertyChangedEventHandler? PropertyChanged;

    public string Xuid { get; } = profile.Xuid;

    public string Game { get; } = profile.Game;

    /// <summary>The plain name (OD-120).</summary>
    public string Gamertag { get; } = ProfileName.Plain(profile.Gamertag);

    public bool Announce
    {
        get => _announce;
        private set { _announce = value; Changed(); }
    }

    public bool Busy
    {
        get => _busy;
        private set { _busy = value; Changed(); Changed(nameof(Idle)); }
    }

    public bool Idle => !_busy;

    /// <summary>
    /// Sets the switch to <paramref name="wanted"/> through <paramref name="put"/>. While a change is in flight, or when nothing changes,
    /// nothing is sent (the view is told the current value again, so a clicked box snaps back).
    /// </summary>
    public async Task SetAsync(bool wanted, Func<bool, CancellationToken, Task<ProfileAnnounce>> put, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(put);
        if (_busy || wanted == _announce) { Changed(nameof(Announce)); return; }
        var before = _announce;
        Announce = wanted;
        Busy = true;
        try
        {
            var answer = await put(wanted, cancellation).ConfigureAwait(true);
            Announce = answer.Announce;
        }
        catch
        {
            Announce = before;
            throw;
        }
        finally
        {
            Busy = false;
        }
    }

    private void Changed([CallerMemberName] string? name = null) => PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(name));
}

/// <summary>The Discord routes' refusals in the player's words (an i18n key), before the generic <c>error.&lt;CODE&gt;</c> text.</summary>
public static class DiscordErrors
{
    /// <summary>The key for <paramref name="error"/>, or null to show the code's generic text.</summary>
    public static string? TextKey(ApiException error)
    {
        ArgumentNullException.ThrowIfNull(error);
        return error.Code switch
        {
            ErrorCodes.ServiceUnavailable => "discord.unavailable",
            ErrorCodes.AccountsDeletionPending => "discord.deletion_pending",
            ErrorCodes.RequestRateLimited => RetrySeconds(error) is null ? "discord.rate_limited" : "discord.rate_limited_wait",
            ErrorCodes.AccountsNotFound => "discord.profile_gone",
            _ => null,
        };
    }

    /// <summary>A 429's <c>Retry-After</c> in whole seconds (rounded up, at least 1), or null.</summary>
    public static int? RetrySeconds(ApiException error) =>
        error?.RetryAfter is { } wait && wait > TimeSpan.Zero ? Math.Max(1, (int)Math.Ceiling(wait.TotalSeconds)) : null;
}

/// <summary>What the launcher accepts from the server's Discord answers.</summary>
public static class DiscordLinks
{
    /// <summary>The bot's link page: absolute https (plain http only to a development host, OD-34), no user info; else null.</summary>
    public static Uri? LinkPage(string? url)
    {
        if (string.IsNullOrWhiteSpace(url) || !Uri.TryCreate(url, UriKind.Absolute, out var page) || page.UserInfo.Length > 0) return null;
        if (page.Scheme == Uri.UriSchemeHttps) return page;
        return page.Scheme == Uri.UriSchemeHttp && DevNet.PlainHttpUrl(page.GetLeftPart(UriPartial.Authority)) ? page : null;
    }

    /// <summary>The avatar is fetched only from Discord's CDN over https (the server builds it there, CORE_API.md §5a.2); else null.</summary>
    public static Uri? Avatar(string? url) =>
        !string.IsNullOrWhiteSpace(url) && Uri.TryCreate(url, UriKind.Absolute, out var avatar) && avatar.Scheme == Uri.UriSchemeHttps
            && avatar.IsDefaultPort && avatar.UserInfo.Length == 0 && string.Equals(avatar.IdnHost, "cdn.discordapp.com", StringComparison.OrdinalIgnoreCase)
            ? avatar : null;

    /// <summary>The handle as shown: <c>Global Name (@username)</c>, or <c>@username</c> without a display name.</summary>
    public static string Handle(DiscordLink link)
    {
        ArgumentNullException.ThrowIfNull(link);
        return string.IsNullOrWhiteSpace(link.GlobalName) || link.GlobalName == link.Username
            ? "@" + link.Username : link.GlobalName + " (@" + link.Username + ")";
    }

    /// <summary>The avatar's fallback: the first letter of the display name (or the username), upper case.</summary>
    public static string Initial(DiscordLink link)
    {
        ArgumentNullException.ThrowIfNull(link);
        var name = string.IsNullOrWhiteSpace(link.GlobalName) ? link.Username : link.GlobalName;
        var first = name.EnumerateRunes().FirstOrDefault(r => Rune.IsLetterOrDigit(r));
        return first.Value == 0 ? "?" : Rune.ToUpperInvariant(first).ToString();
    }
}

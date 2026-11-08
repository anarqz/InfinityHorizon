// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net;
using Infinity.Core.Api;
using Infinity.Core.Online;

namespace Infinity.Core.Shell;

/// <summary>The header's friends button (owner 2026-11-08): "{icon} online/total", and the green dot for anything pending.</summary>
/// <param name="Online">Friends online now.</param>
/// <param name="Total">All friends.</param>
/// <param name="Pending">An incoming friend request, or a friend's live invite to this account, waits for the player.</param>
public sealed record FriendsSummary(int Online, int Total, bool Pending)
{
    public static FriendsSummary None { get; } = new(0, 0, false);

    /// <summary>"3/12".</summary>
    public string Counts => Online.ToString(System.Globalization.CultureInfo.InvariantCulture) + "/" + Total.ToString(System.Globalization.CultureInfo.InvariantCulture);
}

/// <summary>
/// Owner 2026-11-08: the launcher's FRIENDS sidebar, a shell-level right pane toggled from the header, matching the in-game overlay's sidebar
/// (W15, CORE_API §4.1/§4.2a): the same <see cref="FriendList"/> from the same client, no new route.
/// </summary>
public static class LauncherFriends
{
    /// <summary>The counts and the pending dot of <paramref name="list"/>.</summary>
    public static FriendsSummary Summary(FriendList? list)
    {
        if (list is null) return FriendsSummary.None;
        return new FriendsSummary(list.Friends.Count(f => f.Online), list.Friends.Count,
            list.Incoming.Count > 0 || list.Friends.Any(f => f.Invite is not null));
    }

    /// <summary>The name a friend shows: the profile it plays now when online, else its first profile.</summary>
    public static string ShownName(Friend friend)
    {
        ArgumentNullException.ThrowIfNull(friend);
        if (friend.Online && friend.Playing is { Gamertag.Length: > 0 } playing) return playing.Gamertag;
        return (friend.Profiles.Count > 0 ? friend.Profiles[0].Gamertag : null) ?? friend.Playing?.Gamertag ?? string.Empty;
    }

    /// <summary>The list's order: online first, then by the shown name (case-insensitive, then ordinal), then by id.</summary>
    public static IReadOnlyList<Friend> Sorted(FriendList list)
    {
        ArgumentNullException.ThrowIfNull(list);
        return list.Friends.OrderBy(f => f.Online ? 0 : 1)
            .ThenBy(ShownName, StringComparer.OrdinalIgnoreCase)
            .ThenBy(ShownName, StringComparer.Ordinal)
            .ThenBy(f => f.Id, StringComparer.Ordinal).ToList();
    }
}

/// <summary>
/// The friends sidebar's open/closed state, remembered across launches (owner 2026-11-08: closed by default). One small file in the
/// launcher's state folder.
/// </summary>
public static class FriendsSidebarStore
{
    private const string FileName = "friends-sidebar";

    public static bool ReadOpen(string stateDirectory)
    {
        try
        {
            var path = Path.Combine(stateDirectory, FileName);
            return File.Exists(path) && new FileInfo(path).Length <= 16 && File.ReadAllText(path).Trim() == "open";
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return false; }
    }

    public static void WriteOpen(string stateDirectory, bool open)
    {
        Directory.CreateDirectory(stateDirectory);
        var path = Path.Combine(stateDirectory, FileName);
        File.WriteAllText(path + ".tmp", open ? "open" : "closed");
        File.Move(path + ".tmp", path, overwrite: true);
    }
}

/// <summary>
/// The launcher's presence poll of the friends list while signed in (owner 2026-11-08: the header's counts and dot come from polling, never
/// from a click): <c>GET /launcher/v1/friends</c> every <see cref="PollEvery"/>; a 429 waits its <c>Retry-After</c> (at least the backoff);
/// other failures back off up to <see cref="MaxBackoff"/>; 401/403, an online pause or a signed-out token source stop it for good. It never
/// reads <c>/launcher/v1/invites</c>, whose <c>online[]</c> belongs to the game session's inbox (its toasts).
/// </summary>
public sealed class FriendsPoller(Func<CancellationToken, Task<FriendList>> read, Action<FriendList> seen, TimeProvider? clock = null, Action<string>? log = null)
{
    public static readonly TimeSpan PollEvery = TimeSpan.FromSeconds(30);

    public static readonly TimeSpan MaxBackoff = TimeSpan.FromMinutes(5);

    private readonly Func<CancellationToken, Task<FriendList>> _read = read ?? throw new ArgumentNullException(nameof(read));
    private readonly Action<FriendList> _seen = seen ?? throw new ArgumentNullException(nameof(seen));
    private readonly TimeProvider _clock = clock ?? TimeProvider.System;
    private readonly Action<string> _log = log ?? (_ => { });

    /// <summary>Polling stopped for good (refused, paused or signed out).</summary>
    public bool Stopped { get; private set; }

    /// <summary>The wait before the next poll after <paramref name="failures"/> failures in a row (0: <see cref="PollEvery"/>).</summary>
    public static TimeSpan Backoff(int failures) =>
        failures <= 0 ? PollEvery : TimeSpan.FromTicks(Math.Min(PollEvery.Ticks << Math.Min(failures, 8), MaxBackoff.Ticks));

    /// <summary>The wait after one answer: the server's <c>Retry-After</c> wins when longer than the backoff.</summary>
    public static TimeSpan WaitAfter(ApiException? error, int failures)
    {
        var backoff = Backoff(failures);
        return error is { Status: HttpStatusCode.TooManyRequests, RetryAfter: { } retry } && retry > backoff ? retry : backoff;
    }

    public async Task RunAsync(CancellationToken cancellation)
    {
        var failures = 0;
        var wait = TimeSpan.Zero;
        try
        {
            while (!cancellation.IsCancellationRequested)
            {
                if (wait > TimeSpan.Zero) await Task.Delay(wait, _clock, cancellation).ConfigureAwait(true);
                try
                {
                    var list = await _read(cancellation).ConfigureAwait(true);
                    failures = 0;
                    _seen(list);
                    wait = PollEvery;
                }
                catch (ApiException error) when (error.Status is HttpStatusCode.Unauthorized or HttpStatusCode.Forbidden || OnlinePause.From(error) is not null)
                {
                    _log($"friends: presence poll stopped (HTTP {(int)error.Status} {error.Code})");
                    Stopped = true;
                    return;
                }
                catch (InvalidOperationException) when (!cancellation.IsCancellationRequested)
                {
                    _log("friends: presence poll stopped (signed out)");
                    Stopped = true;
                    return;
                }
                catch (ApiException error) when (!cancellation.IsCancellationRequested)
                {
                    wait = WaitAfter(error, ++failures);
                    _log($"friends: presence poll answered HTTP {(int)error.Status} {error.Code}; next in {(int)wait.TotalSeconds} s");
                }
                catch (Exception error) when (!cancellation.IsCancellationRequested && error is HttpRequestException or IOException
                    or OperationCanceledException or System.Text.Json.JsonException or TimeoutException)
                {
                    wait = Backoff(++failures);
                    _log($"friends: presence poll failed ({error.GetType().Name}); next in {(int)wait.TotalSeconds} s");
                }
            }
        }
        catch (OperationCanceledException) when (cancellation.IsCancellationRequested)
        {
        }
    }
}

// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Api;
using Infinity.Core.Online;

namespace Infinity.Core.Matchmaking;

/// <summary>
/// The Worlds tab's friend requests and reports (OD-63) through core, with the launcher session's
/// access token: each answer becomes the <c>worlds.*</c> key the overlay shows. The tier is never
/// guessed from a refusal (a 404 is "not found" for every reason, CORE_API.md §4.1).
/// </summary>
public sealed class WorldSocial(ApiClient api, Func<CancellationToken, Task<string>> access, string game = "fh2") : IWorldSocial
{
    private readonly ApiClient _api = api ?? throw new ArgumentNullException(nameof(api));
    private readonly Func<CancellationToken, Task<string>> _access = access ?? throw new ArgumentNullException(nameof(access));

    public async Task<(bool Ok, string Key)> AddFriendAsync(string gamertag, CancellationToken cancellation)
    {
        try
        {
            var token = await _access(cancellation).ConfigureAwait(false);
            var result = await _api.AddFriendAsync(token, game, gamertag, cancellation).ConfigureAwait(false);
            return (true, result.State == "friends" ? "worlds.friend_now" : "worlds.friend_requested");
        }
        catch (ApiException e) when (OnlinePause.From(e) is null)
        {
            return (false, FriendRefusal(e.Code));
        }
        catch (Exception e) when (e is HttpRequestException or TaskCanceledException or InvalidOperationException)
        {
            return (false, "worlds.action_failed");
        }
    }

    public async Task<(bool Ok, string Key)> ReportAsync(string xuid, string reason, string? worldId, CancellationToken cancellation)
    {
        try
        {
            var token = await _access(cancellation).ConfigureAwait(false);
            await _api.ReportAsync(token, new GameReport { Game = game, Xuid = xuid, Reason = reason, WorldId = worldId }, cancellation).ConfigureAwait(false);
            return (true, "worlds.reported");
        }
        catch (ApiException e) when (OnlinePause.From(e) is null)
        {
            return (false, e.Code == ErrorCodes.RequestRateLimited ? "worlds.slow_down" : "worlds.report_failed");
        }
        catch (Exception e) when (e is HttpRequestException or TaskCanceledException or InvalidOperationException)
        {
            return (false, "worlds.report_failed");
        }
    }

    /// <summary>The overlay's words for a refused friend request.</summary>
    public static string FriendRefusal(string code) => code switch
    {
        "PORTAL.NOT_FOUND" => "worlds.friend_not_found",
        "SOCIAL.LIMIT_REACHED" => "worlds.friend_limit",
        ErrorCodes.RequestRateLimited => "worlds.slow_down",
        _ => "worlds.action_failed",
    };
}

// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Api;

/// <summary>Core's social reads for the launcher (CORE_API.md §4.1-§4.2), with the access token.</summary>
public sealed partial class ApiClient
{
    /// <summary>The 10 s cadence the contract suggests for the invites inbox.</summary>
    public static readonly TimeSpan InvitesPollEvery = TimeSpan.FromSeconds(10);

    /// <summary>
    /// <c>GET /launcher/v1/friends[?asXuid=]</c>: friends with their tier and presence;
    /// <paramref name="asXuid"/> (a live profile of the caller's account) adds <c>playing.joinable</c>.
    /// </summary>
    public Task<FriendList> FriendsAsync(string access, string? asXuid = null, CancellationToken cancellation = default) =>
        SendAsync<FriendList>(HttpMethod.Get, Core, "/launcher/v1/friends" + AsXuid(asXuid), null, access, idempotent: false, cancellation);

    /// <summary>
    /// <c>GET /launcher/v1/invites[?asXuid=]</c>: pending invites to the account and requests to join
    /// its world; <paramref name="asXuid"/> decides <c>sameMode</c> (else the active session's profile).
    /// Open to every account (OD-129); a 403 means stop polling.
    /// </summary>
    public Task<InviteList> InvitesAsync(string access, string? asXuid = null, CancellationToken cancellation = default) =>
        SendAsync<InviteList>(HttpMethod.Get, Core, "/launcher/v1/invites" + AsXuid(asXuid), null, access, idempotent: false, cancellation);

    /// <summary><c>POST /launcher/v1/invites/{inviteId}/decline</c> → 204, either kind; idempotent by state (no <c>Idempotency-Key</c>).</summary>
    public async Task DeclineInviteAsync(string access, string inviteId, CancellationToken cancellation = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(inviteId);
        await SendAsync<System.Text.Json.JsonElement?>(HttpMethod.Post, Core, $"/launcher/v1/invites/{Uri.EscapeDataString(inviteId)}/decline", null, access, idempotent: false, cancellation).ConfigureAwait(false);
    }

    /// <summary><c>POST /launcher/v1/friends {game, gamertag}</c> → 200 <see cref="FriendAddResult"/> (CORE_API.md §4.1; core requires an <c>Idempotency-Key</c>).</summary>
    public Task<FriendAddResult> AddFriendAsync(string access, string game, string gamertag, CancellationToken cancellation = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(gamertag);
        return SendAsync<FriendAddResult>(HttpMethod.Post, Core, "/launcher/v1/friends", new FriendAdd { Game = game, Gamertag = gamertag }, access, idempotent: true, cancellation);
    }

    /// <summary>
    /// <c>POST /launcher/v1/reports {game, xuid, reason, worldId?}</c> → 202 <c>{id}</c> (CR-L25, OD-63): a
    /// player the reporter shared a world with in the last 30 minutes; one per target a day.
    /// </summary>
    public Task<GameReportResult> ReportAsync(string access, GameReport report, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(report);
        return SendAsync<GameReportResult>(HttpMethod.Post, Core, "/launcher/v1/reports", report, access, idempotent: true, cancellation);
    }

    private static string AsXuid(string? asXuid) =>
        string.IsNullOrEmpty(asXuid) ? string.Empty : "?asXuid=" + Uri.EscapeDataString(asXuid);
}

// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Api;

/// <summary>Core's social reads for the launcher (CORE_API.md §4.1-§4.2), with the access token.</summary>
public sealed partial class ApiClient
{
    /// <summary>The 10 s cadence the contract suggests for the invites inbox.</summary>
    public static readonly TimeSpan InvitesPollEvery = TimeSpan.FromSeconds(10);

    /// <summary>
    /// <c>GET /launcher/v1/friends[?asXuid=]</c>: friends with their tier and presence;
    /// <paramref name="asXuid"/> (a live profile of the caller's account) adds the server's <c>playing.joinable</c> (OD-208: not read; a player world's).
    /// </summary>
    public Task<FriendList> FriendsAsync(string access, string? asXuid = null, CancellationToken cancellation = default) =>
        SendAsync<FriendList>(HttpMethod.Get, Core, "/launcher/v1/friends" + AsXuid(asXuid), null, access, idempotent: false, cancellation);

    /// <summary>
    /// <c>GET /launcher/v1/invites[?asXuid=]</c>: pending invites to the account and requests to join
    /// its world; <paramref name="asXuid"/> decides <c>sameMode</c> (else the active session's profile).
    /// Open to every account (OD-129); a 403 means stop polling.
    /// </summary>
    public Task<InviteList> InvitesAsync(string access, string? asXuid = null, CancellationToken cancellation = default) =>
        InvitesAsync(access, asXuid, first: false, cancellation);

    /// <summary>
    /// <c>GET /launcher/v1/invites[?asXuid=][&amp;first=true]</c> (CORE_API §4.2a, OD-218): <paramref name="first"/> marks this launcher's
    /// first poll since it started, which names nobody in <c>online</c> (use <see cref="InvitesFirstPoll"/>).
    /// </summary>
    public Task<InviteList> InvitesAsync(string access, string? asXuid, bool first, CancellationToken cancellation = default)
    {
        var query = AsXuid(asXuid);
        if (first) query += (query.Length == 0 ? "?" : "&") + "first=true";
        return SendAsync<InviteList>(HttpMethod.Get, Core, "/launcher/v1/invites" + query, null, access, idempotent: false, cancellation);
    }

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
    /// <c>POST /launcher/v1/friends {game, xuid}</c> → 200 <see cref="FriendAddResult"/> (CR-S89, OD-193): a request to the account
    /// of that profile, or the acceptance of its request. <paramref name="idempotencyKey"/> is the user action's (every retry
    /// of that action reuses it).
    /// </summary>
    public Task<FriendAddResult> AddFriendByXuidAsync(string access, string game, string xuid, string idempotencyKey, CancellationToken cancellation = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(xuid);
        ArgumentException.ThrowIfNullOrWhiteSpace(idempotencyKey);
        return SendAsync<FriendAddResult>(HttpMethod.Post, Core, "/launcher/v1/friends", new FriendAdd { Game = game, Xuid = xuid }, access, idempotencyKey, cancellation);
    }

    /// <summary><c>POST /launcher/v1/friends/{friendId}/accept</c> → 200 <see cref="Friend"/> (CORE_API.md §4.1; any XUID of the asker).</summary>
    public Task<Friend> AcceptFriendAsync(string access, string friendId, string idempotencyKey, CancellationToken cancellation = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(friendId);
        ArgumentException.ThrowIfNullOrWhiteSpace(idempotencyKey);
        return SendAsync<Friend>(HttpMethod.Post, Core, $"/launcher/v1/friends/{Uri.EscapeDataString(friendId)}/accept", null, access, idempotencyKey, cancellation);
    }

    /// <summary>
    /// <c>DELETE /launcher/v1/friends/{friendId}</c> → 204 (CORE_API.md §4.1): unfriends, cancels the caller's request and declines
    /// theirs, whichever is there.
    /// </summary>
    public async Task RemoveFriendAsync(string access, string friendId, string idempotencyKey, CancellationToken cancellation = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(friendId);
        ArgumentException.ThrowIfNullOrWhiteSpace(idempotencyKey);
        await SendAsync<System.Text.Json.JsonElement?>(HttpMethod.Delete, Core, $"/launcher/v1/friends/{Uri.EscapeDataString(friendId)}", null, access, idempotencyKey, cancellation).ConfigureAwait(false);
    }

    /// <summary>
    /// <c>POST /launcher/v1/reports {game, xuid, reason, worldId?}</c> → 202 <c>{id}</c> (CR-L25, OD-63; CR-S90, OD-193): any profile of the
    /// game but one's own; with <c>worldId</c> only a player the reporter shared that world with in the last 30 minutes. One per
    /// target a day.
    /// </summary>
    public Task<GameReportResult> ReportAsync(string access, GameReport report, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(report);
        return SendAsync<GameReportResult>(HttpMethod.Post, Core, "/launcher/v1/reports", report, access, idempotent: true, cancellation);
    }

    /// <summary>As <see cref="ReportAsync(string, GameReport, CancellationToken)"/> with the user action's <c>Idempotency-Key</c> (a retry reuses it).</summary>
    public Task<GameReportResult> ReportAsync(string access, GameReport report, string idempotencyKey, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(report);
        ArgumentException.ThrowIfNullOrWhiteSpace(idempotencyKey);
        return SendAsync<GameReportResult>(HttpMethod.Post, Core, "/launcher/v1/reports", report, access, idempotencyKey, cancellation);
    }

    /// <summary>
    /// <c>POST /launcher/v1/voice/mutes {xuid, action, worldId}</c> → 202 (CR-S68, CORE_API.md §4.4): idempotent by state, so no
    /// <c>Idempotency-Key</c>; the caller retries until 202 (404 <c>VOICE.NOT_SHARED</c> stops, 429 waits its <c>Retry-After</c>).
    /// </summary>
    public Task<VoiceMuteAccepted> VoiceMuteAsync(string access, VoiceMuteReport mute, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(mute);
        return SendAsync<VoiceMuteAccepted>(HttpMethod.Post, Core, "/launcher/v1/voice/mutes", mute, access, idempotent: false, cancellation);
    }

    /// <summary><c>GET /core/v1/me/notices</c> (the launcher's access token, CORE_API.md §2.3): the account's notices, never cached.</summary>
    public Task<AccountNoticeList> AccountNoticesAsync(string access, CancellationToken cancellation = default) =>
        SendAsync<AccountNoticeList>(HttpMethod.Get, Core, "/core/v1/me/notices", null, access, idempotent: false, cancellation);

    private static string AsXuid(string? asXuid) =>
        string.IsNullOrEmpty(asXuid) ? string.Empty : "?asXuid=" + Uri.EscapeDataString(asXuid);
}

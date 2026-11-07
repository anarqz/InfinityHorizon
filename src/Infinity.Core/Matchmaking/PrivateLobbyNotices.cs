// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Overlay;

namespace Infinity.Core.Matchmaking;

/// <summary>
/// OD-204 (T-L3; WORLDS_OFFICIAL §12.2 item 6, §12.5, §12.9 A3, A8, A10; MATCHMAKING_API §2.10): the ONE table of what a private
/// lobby's refusal tells the player, for the private create (T-L6) and, reused as is, the invites (T-L4) and the invitee's join
/// (T-L5). Each server reason maps to one bilingual i18n-win key; nothing else in the client words these refusals.
/// <para>
/// The notice rule (fixes T-L6's M1): every notice built here is marked <see cref="UiToastFlags.Always"/>, so it shows in an official
/// lobby, a private one included; OD-192's quiet rule (<see cref="Supervisor.OverlayChannel.Quiet"/>, the overlay's PushToast)
/// still silences every other notice there. Invite prompts are their own modal frame (<see cref="UiFrame.InvitePrompt"/>) and
/// always show.
/// </para>
/// </summary>
public static class PrivateLobbyNotices
{
    /// <summary>409 <c>not_leader</c>: an invite from a player who no longer (or never) leads the lobby (A3, §12.8b).</summary>
    public const string NotLeader = ConflictReasons.NotLeader;

    /// <summary>409 <c>region</c>: an invite to a friend in another region (A8, OD-205).</summary>
    public const string Region = ConflictReasons.Region;

    /// <summary>200 <c>not_allowed {reason: "friends"}</c>: not a friend of the current leader any more (an unfriended player's rejoin, A2/A3).</summary>
    public const string Friends = "friends";

    /// <summary>An older server's <c>REQUEST.INVALID {field: "private"}</c>: no private lobbies there yet.</summary>
    public const string Unsupported = "private_unsupported";

    /// <summary>A 429 or <c>REQUEST.RATE_LIMITED</c>: slow down.</summary>
    public const string RateLimited = "rate_limited";

    /// <summary>No answer in time, or a failure with no reason.</summary>
    public const string Failed = "failed";

    /// <summary>T-L4: 409 <c>full</c> on an invite: pending invites already hold every free seat (A6).</summary>
    public const string Full = ConflictReasons.Full;

    /// <summary>T-L4: 409 <c>pending_limit</c> on an invite (MATCHMAKING_API §3.4; §2.10 says a private world has none, read anyway).</summary>
    public const string PendingLimit = ConflictReasons.PendingLimit;

    /// <summary>T-L4: 404 <c>PORTAL.NOT_FOUND</c> on an invite: not a friend of the leader's account (one answer for every reason).</summary>
    public const string NotFriend = "not_friend";

    /// <summary>T-L4: the lobby closed (404 <c>MM.WORLD_NOT_FOUND</c>) before the invite.</summary>
    public const string Closed = "closed";

    /// <summary>T-L4: Invite friends outside a private lobby (the game's own menu, or a stale pane).</summary>
    public const string NotPrivate = "not_private";

    /// <summary>T-L4: an invite that failed with no reason the table knows (a 5xx, a transport failure, a malformed answer).</summary>
    public const string InviteFailed = "invite_failed";

    /// <summary>T-L4: the invite was made (201) or was already pending (200): "Invite sent to {name}".</summary>
    public const string InviteSent = "invite_sent";

    /// <summary>T-L4: the decline route failed (the invite then expires on its own in 2 minutes).</summary>
    public const string DeclineFailed = "decline_failed";

    /// <summary>The one table: a server reason (or one of this class's own) and the i18n-win key of its notice.</summary>
    public static IReadOnlyDictionary<string, string> Keys { get; } = new Dictionary<string, string>(StringComparer.Ordinal)
    {
        [Full] = "worlds.invite_full",
        [PendingLimit] = "worlds.invite_pending_limit",
        [NotFriend] = "worlds.invite_not_friend",
        [Closed] = "worlds.closed",
        [NotPrivate] = "worlds.invite_private_only",
        [InviteFailed] = "worlds.invite_failed",
        [InviteSent] = "worlds.invite_sent", // with {name}
        [DeclineFailed] = "worlds.invite_decline_failed",
        [ConflictReasons.LobbiesFull] = "worlds.lobbies_full", // also a create that would take the last public spare
        [ConflictReasons.Pool] = "worlds.private_pool", // A10, with {time}; without a time: PoolNoTimeKey
        [ConflictReasons.ClientUpdate] = "worlds.client_update",
        [NotLeader] = "worlds.private_not_leader",
        [Region] = "worlds.private_region",
        [ConflictReasons.Activity] = "worlds.private_activity",
        [Friends] = "worlds.private_friends",
        [Unsupported] = "worlds.private_unavailable",
        [RateLimited] = "worlds.slow_down",
        [Failed] = "worlds.private_failed",
    };

    /// <summary>A10 when the flag's end is not known: no time to show.</summary>
    public const string PoolNoTimeKey = "worlds.private_pool_flagged";

    /// <summary>Whether <paramref name="reason"/> is in the table.</summary>
    public static bool Handles(string? reason) => reason is not null && Keys.ContainsKey(reason);

    /// <summary>
    /// The notice's text for <paramref name="reason"/> (an unknown one reads as <see cref="Failed"/>); <paramref name="poolLeft"/> is the
    /// flag's time left for <c>pool</c> (A10: "no online lobby for &lt;time left&gt;"), null when not known.
    /// </summary>
    public static string Text(Func<string, string> text, string reason, TimeSpan? poolLeft = null)
    {
        ArgumentNullException.ThrowIfNull(text);
        if (reason == ConflictReasons.Pool)
            return poolLeft is { } left && left > TimeSpan.Zero
                ? text(Keys[ConflictReasons.Pool]).Replace("{time}", AntiCheatPool.TimeLeft(left), StringComparison.Ordinal)
                : text(PoolNoTimeKey);
        return text(Keys.TryGetValue(reason, out var key) ? key : Keys[Failed]);
    }

    /// <summary>The notice itself: a warning that always shows (<see cref="UiToastFlags.Always"/>), 6 s before OD-192's halving.</summary>
    public static UiFrame.Toast Toast(string text) => new(UiLevel.Warn, 2, 60, text, UiToastFlags.Always);

    /// <summary>T-L4: a good result (the invite sent): an info notice (the overlay's success look) that always shows.</summary>
    public static UiFrame.Toast Done(string text) => new(UiLevel.Info, 1, 60, text, UiToastFlags.Always);

    /// <summary>
    /// T-L4: the reason of a refused invite (<c>POST /mm/v1/worlds/{worldId}/invites</c>, MATCHMAKING_API §2.10, §3.4), a key of this
    /// table: 409 <c>not_leader</c>, <c>full</c>, <c>pending_limit</c>, <c>region</c>, <c>client_update</c>, <c>friends</c> (any other 409
    /// is <see cref="InviteFailed"/>); 403 <c>MM.NOT_HOST</c> (not a private official world, or no owner rights) reads as
    /// <see cref="NotLeader"/>; 404 <c>PORTAL.NOT_FOUND</c> <see cref="NotFriend"/>; 404 <c>MM.WORLD_NOT_FOUND</c> <see cref="Closed"/>;
    /// 429 or <c>REQUEST.RATE_LIMITED</c> <see cref="RateLimited"/>; anything else <see cref="InviteFailed"/>.
    /// </summary>
    public static string InviteRefusal(MmError error)
    {
        ArgumentNullException.ThrowIfNull(error);
        return error.Kind switch
        {
            MmErrorKind.Conflict => error.Reason switch
            {
                NotLeader or Full or PendingLimit or Region or ConflictReasons.ClientUpdate or Friends => error.Reason,
                _ => InviteFailed,
            },
            MmErrorKind.NotHost => NotLeader,
            MmErrorKind.NotFound => NotFriend,
            MmErrorKind.WorldNotFound => Closed,
            MmErrorKind.RateLimited => RateLimited,
            _ => InviteFailed,
        };
    }

    /// <summary>The notice for <paramref name="reason"/>.</summary>
    public static UiFrame.Toast Toast(Func<string, string> text, string reason, TimeSpan? poolLeft = null) => Toast(Text(text, reason, poolLeft));

    /// <summary>A 409 <c>pool</c>'s <c>details.until</c> (<see cref="MmError.Until"/>); null for any other error, or when absent.</summary>
    public static DateTimeOffset? PoolUntil(MmError? error) =>
        error is { Kind: MmErrorKind.Conflict, Reason: ConflictReasons.Pool } ? error.Until : null;
}

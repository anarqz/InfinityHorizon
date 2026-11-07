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

    /// <summary>The one table: a server reason (or one of this class's own) and the i18n-win key of its notice.</summary>
    public static IReadOnlyDictionary<string, string> Keys { get; } = new Dictionary<string, string>(StringComparer.Ordinal)
    {
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

    /// <summary>The notice for <paramref name="reason"/>.</summary>
    public static UiFrame.Toast Toast(Func<string, string> text, string reason, TimeSpan? poolLeft = null) => Toast(Text(text, reason, poolLeft));

    /// <summary>A 409 <c>pool</c>'s <c>details.until</c> (<see cref="MmError.Until"/>); null for any other error, or when absent.</summary>
    public static DateTimeOffset? PoolUntil(MmError? error) =>
        error is { Kind: MmErrorKind.Conflict, Reason: ConflictReasons.Pool } ? error.Until : null;
}

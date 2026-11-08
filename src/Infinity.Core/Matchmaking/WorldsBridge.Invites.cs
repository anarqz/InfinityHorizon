// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Text;
using Infinity.Core.Api;
using Infinity.Core.Overlay;

namespace Infinity.Core.Matchmaking;

/// <summary>
/// What Invite friends answers (T-L4): the friend picker to send the overlay, or one notice; neither while another press is answered.
/// INVITE_UX (protocol 12): <c>Sidebar</c> instead of a picker: send <see cref="WorldsBridge.FriendsStateFrame"/> (read again just now) and
/// <see cref="UiFrame.FriendsOpen"/> with <see cref="UiFriendsOpen.Invite"/>.
/// </summary>
public sealed record InviteFriendsAnswer(UiFrame.FriendPicker? Picker, UiFrame.Toast? Notice, bool Sidebar = false);

/// <summary>What one chosen friend answers (T-L4): one notice, and the refreshed picker (null when there is none to refresh).</summary>
public sealed record InviteSendAnswer(UiFrame.Toast Notice, UiFrame.FriendPicker? Picker);

// T-L4 (OD-204, WORLDS_OFFICIAL §12.5, §12.8b, §12.9 A3/A6/A8; MATCHMAKING_API §2.10, §3.4): the private lobby's friend picker and its
// invites. The game's Invite friends (the hook's InviteFriends, 0x29) and the Private Lobby pane's button (0x26) both come here:
// - in a private lobby whose current leader is this player (PrivateLobbyPolicy.MayInvite, the overlay's InviteGateOf), the picker
//   opens with the player's friends from PlayerActions' friend list (CORE_API §4.1, the T-F2 cache): friends only, each marked in
//   this lobby, invited (this launcher's pending invite) or offline; only an available one may be chosen;
// - in a private lobby led by someone else, or whose leader is not known: one notice, "Only the lobby leader can invite";
// - outside a private lobby: one notice, "Invite friends works in a private lobby".
// A chosen friend (0x27) is POST /mm/v1/worlds/{worldId}/invites {xuid} with one Idempotency-Key per choice (its one transport retry
// reuses it); every answer is one notice from PrivateLobbyNotices' table, marked Always (OD-192's quiet rule holds in official lobbies).
public sealed partial class WorldsBridge
{
    /// <summary>The game whose profiles an invite names (CORE_API §4.1 <c>FriendProfile.game</c>).</summary>
    public const string InviteGame = "fh2";

    /// <summary>T-L4: the invitee's inbox (CORE_API §4.2), polled by the supervisor while online; null: no prompts in this session.</summary>
    public InviteInbox? Inbox { get; init; }

    // this launcher's pending invites: the friend account (its handle) → the world and when the invite lapses (MATCHMAKING_API §3.4: 2 min)
    // R-1108-I2: and when it was sent (a seat read answered just after cannot clear it)
    private readonly Dictionary<string, (string World, DateTimeOffset Until, DateTimeOffset Sent)> _invitesSent = new(StringComparer.Ordinal);

    /// <summary>R-1108-I2: a seat read within this long of an invite never clears its "Invited" mark (it may predate the invite).</summary>
    public static readonly TimeSpan InviteHoldSettle = TimeSpan.FromSeconds(5);

    /// <summary>
    /// R-1108-I2 (SAURON 2026-11-08): the picker's and the FRIENDS sidebar's "Invited" is this launcher's own mark, which only lapsed
    /// with the invite's 2 minutes, so a declined invite left the friend greyed and not invitable. No route lists a leader's sent
    /// invites (MATCHMAKING_API §3.4), but every pending invite holds a seat: <c>openSeats</c> = <c>maxPlayers</c> − <c>players</c> −
    /// reserved and held seats (§2.1). A seat read of <paramref name="worldId"/> with none reserved or held means no invite into it
    /// is pending any more (declined, lapsed or used): every mark into it older than <see cref="InviteHoldSettle"/> goes, and those
    /// friends may be invited again. Called with each seat read and renewal.
    /// </summary>
    internal void ObserveInviteHolds(string worldId, WorldView world)
    {
        if (world.Players + (ulong)world.OpenSeats < world.MaxPlayers) return; // some seat is still held or reserved
        var settled = Now - InviteHoldSettle;
        int cleared;
        lock (_invitesSent)
        {
            var gone = _invitesSent.Where(e => e.Value.World == worldId && e.Value.Sent <= settled).Select(e => e.Key).ToList();
            foreach (var key in gone) _invitesSent.Remove(key);
            cleared = gone.Count;
        }
        if (cleared > 0) _log($"worlds: invite: {cleared} invite mark(s) cleared (the world holds no seat for an invite any more: declined, lapsed or used)");
    }

    /// <summary>
    /// The game's Invite friends (<paramref name="fromGame"/>) or the Private Lobby pane's: the picker to open, or the notice saying
    /// why not. The lobby is read again first (bounded); a failed read uses the last one. <paramref name="sidebar"/> (an overlay of
    /// protocol 12, INVITE_UX): the friend list is read again and the answer says to open the FRIENDS sidebar's invite mode instead.
    /// </summary>
    public async Task<InviteFriendsAnswer> InviteFriendsAsync(bool fromGame, CancellationToken cancellation, bool sidebar = false)
    {
        // review L6: a press while one is answered is the same press (no second picker, no stacked notice)
        if (Interlocked.Exchange(ref _inviteFriendsBusy, 1) != 0) return new(null, null);
        try { return await InviteFriendsCoreAsync(fromGame, sidebar, cancellation).ConfigureAwait(false); }
        finally { Volatile.Write(ref _inviteFriendsBusy, 0); }
    }

    private int _inviteFriendsBusy;

    private async Task<InviteFriendsAnswer> InviteFriendsCoreAsync(bool fromGame, bool sidebar, CancellationToken cancellation)
    {
        var mine = await CurrentLobbyAsync(cancellation).ConfigureAwait(false);
        var where = fromGame ? "the game's Invite friends" : "the Private Lobby pane's Invite friends";
        if (!InPrivateMine(mine))
        {
            _log($"worlds: invite: {where} outside a private lobby");
            return new(null, PrivateLobbyNotices.Toast(_text, PrivateLobbyNotices.NotPrivate));
        }
        if (!PrivateLobbyPolicy.MayInvite(mine.Leader, _ownXuid))
        {
            _log($"worlds: invite: {where} in {mine.WorldId}: this player is not its leader ({(mine.Leader.Length == 0 ? "the leader is not known" : "another player leads")})");
            return new(null, PrivateLobbyNotices.Toast(_text, PrivateLobbyNotices.NotLeader));
        }
        if (sidebar)
        {
            var read = await ReadFriendsAsync(force: true, cancellation).ConfigureAwait(false);
            _log($"worlds: invite: {where}: the FRIENDS sidebar opens to invite ({(read ? "the list read again" : "the friend list could not be read")})");
            return new(null, null, Sidebar: true);
        }
        var picker = await PickerAsync(mine, UiPickerFlags.Open, cancellation).ConfigureAwait(false);
        _log($"worlds: invite: {where}: the friend picker opens ({(picker.ListStatus == UiPickerStatus.List ? picker.Friends.Count.ToString(CultureInfo.InvariantCulture) + " friends" : "the friend list could not be read")})");
        return new(picker, null);
    }

    /// <summary>
    /// A friend chosen in the picker: one invite call (<see cref="IMatchmakingApi.InviteAsync(string, string, string, CancellationToken)"/>)
    /// and one notice for its answer. Nothing is sent outside a private lobby this player leads, nor to anyone not on the friend list.
    /// </summary>
    public async Task<InviteSendAnswer> InviteSendAsync(string xuid, CancellationToken cancellation)
    {
        ArgumentException.ThrowIfNullOrEmpty(xuid);
        var mine = Volatile.Read(ref _mineSnapshot);
        if (!InPrivateMine(mine))
            return new(PrivateLobbyNotices.Toast(_text, PrivateLobbyNotices.NotPrivate), null);
        if (!PrivateLobbyPolicy.MayInvite(mine.Leader, _ownXuid))
            return new(PrivateLobbyNotices.Toast(_text, PrivateLobbyNotices.NotLeader), null);
        // friends only (the picker's own list, read again when older than PlayerActions.FreshFor)
        if (PlayerActions is { } actions) await actions.RefreshAsync(force: false, cancellation).ConfigureAwait(false);
        var friend = FriendOf(PlayerActions?.Cached, xuid);
        if (friend is null)
        {
            _log("worlds: invite: the chosen player is not on the friend list; nothing sent");
            return new(PrivateLobbyNotices.Toast(_text, PrivateLobbyNotices.NotFriend), await RefreshedPickerAsync(mine, cancellation).ConfigureAwait(false));
        }
        var name = GamertagOf(friend, xuid);
        var key = Guid.CreateVersion7().ToString(); // one per choice; the retry below reuses it
        MmResult<Invite>? answer = null;
        string reason;
        try
        {
            try
            {
                answer = await _mm.InviteAsync(mine.WorldId, xuid, key, cancellation).ConfigureAwait(false);
            }
            catch (HttpRequestException) when (!cancellation.IsCancellationRequested)
            {
                answer = await _mm.InviteAsync(mine.WorldId, xuid, key, cancellation).ConfigureAwait(false);
            }
            reason = answer.Ok ? PrivateLobbyNotices.InviteSent : PrivateLobbyNotices.InviteRefusal(answer.Error!);
        }
        catch (Exception error) when (HostFailure(error) && !cancellation.IsCancellationRequested)
        {
            _log($"worlds: invite: failed ({error.GetType().Name})");
            reason = PrivateLobbyNotices.InviteFailed;
        }
        UiFrame.Toast notice;
        if (reason == PrivateLobbyNotices.InviteSent)
        {
            var until = answer?.Value is { } made && DateTimeOffset.TryParse(made.ExpiresAt, CultureInfo.InvariantCulture,
                DateTimeStyles.AssumeUniversal | DateTimeStyles.AdjustToUniversal, out var expires) ? expires : Now + TimeSpan.FromSeconds(MatchmakingLimits.InviteTtlSecs);
            lock (_invitesSent) _invitesSent[friend.Id] = (mine.WorldId, until, Now);
            _log($"worlds: invite: sent in {mine.WorldId} ({(answer!.Created ? "new" : "already pending")})");
            notice = PrivateLobbyNotices.Done(PrivateLobbyNotices.Text(_text, PrivateLobbyNotices.InviteSent).Replace("{name}", name, StringComparison.Ordinal));
        }
        else
        {
            _log($"worlds: invite: refused ({reason}{(answer?.Error is { } refused ? $", HTTP {(int)refused.Status} {refused.Code}" : string.Empty)})");
            notice = PrivateLobbyNotices.Toast(_text, reason);
        }
        return new(notice, await RefreshedPickerAsync(mine, cancellation).ConfigureAwait(false));
    }

    /// <summary>
    /// The picker's rows from <paramref name="list"/> (friends only; their requests never appear): one row per friend account with a
    /// profile of <see cref="InviteGame"/> (the one it plays now, else its first), at most <see cref="UiFrame.MaxPickerFriends"/>,
    /// available ones first, then invited, in this lobby, offline, each group in the list's own presence order.
    /// <paramref name="invited"/>: the friend accounts this launcher's invite into <paramref name="mine"/> is pending for.
    /// </summary>
    public static IReadOnlyList<PickerFriend> PickerRows(FriendList list, UiFrame.MyWorld mine, IReadOnlyCollection<string> invited)
    {
        ArgumentNullException.ThrowIfNull(list);
        ArgumentNullException.ThrowIfNull(mine);
        ArgumentNullException.ThrowIfNull(invited);
        var rows = new List<PickerFriend>();
        var seen = new HashSet<string>(StringComparer.Ordinal);
        foreach (var friend in list.Friends)
        {
            if (ProfileOf(friend) is not { } profile || !seen.Add(profile.Xuid)) continue;
            var inLobby = !string.IsNullOrEmpty(mine.WorldId) && (friend.Profiles.Any(p => mine.Members.Any(m => m.Xuid == p.Xuid))
                || friend.Playing is { State: "in_world" } playing && playing.WorldId == mine.WorldId);
            var state = inLobby ? UiPickerState.InLobby
                : invited.Contains(friend.Id) ? UiPickerState.Invited
                : !friend.Online ? UiPickerState.Offline
                : UiPickerState.Available;
            rows.Add(new PickerFriend(profile.Xuid, Cut(profile.Gamertag, 64), state));
        }
        return rows.Where(r => r.Gamertag.Length != 0)
            .OrderBy(r => r.State switch { UiPickerState.Available => 0, UiPickerState.Invited => 1, UiPickerState.InLobby => 2, _ => 3 })
            .Take(UiFrame.MaxPickerFriends).ToList();
    }

    // the profile an invite names: the one the friend plays now in this game, else its first one in this game (a decimal XUID)
    private static (string Xuid, string Gamertag)? ProfileOf(Friend friend)
    {
        if (friend.Playing is { } playing && playing.Game == InviteGame && Decimal(playing.Xuid)) return (playing.Xuid, playing.Gamertag);
        return friend.Profiles.FirstOrDefault(p => p.Game == InviteGame && Decimal(p.Xuid)) is { } first ? (first.Xuid, first.Gamertag) : null;
    }

    private static bool Decimal(string? xuid) => xuid is { Length: > 0 and <= 20 } && xuid.All(char.IsAsciiDigit);

    // the friend account one of whose profiles is xuid (a friend, never a request); null: not on the list (or no list)
    private static Friend? FriendOf(FriendList? list, string xuid) =>
        list?.Friends.FirstOrDefault(f => f.Profiles.Any(p => p.Xuid == xuid) || f.Playing?.Xuid == xuid);

    private static string GamertagOf(Friend friend, string xuid) =>
        friend.Profiles.FirstOrDefault(p => p.Xuid == xuid)?.Gamertag ?? friend.Playing?.Gamertag ?? string.Empty;

    // a private official world this player holds a seat in (as the overlay's MyWorld says)
    private static bool InPrivateMine(UiFrame.MyWorld mine) =>
        mine.Private && mine.Official && mine.Role is UiWorldRole.Seated or UiWorldRole.Reserved && !string.IsNullOrEmpty(mine.WorldId);

    private async Task<UiFrame.MyWorld> CurrentLobbyAsync(CancellationToken cancellation)
    {
        using var bound = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
        bound.CancelAfter(TimeSpan.FromSeconds(5));
        try { return await MyWorldAsync(bound.Token).ConfigureAwait(false); }
        catch (Exception error) when (HostFailure(error) && !cancellation.IsCancellationRequested) { return Volatile.Read(ref _mineSnapshot); }
    }

    private async Task<UiFrame.FriendPicker> PickerAsync(UiFrame.MyWorld mine, byte flags, CancellationToken cancellation)
    {
        var actions = PlayerActions;
        var read = actions is not null && await actions.RefreshAsync(force: false, cancellation).ConfigureAwait(false);
        if (!read || actions!.Cached is not { } list) return new UiFrame.FriendPicker(flags, UiPickerStatus.Unavailable, []);
        return new UiFrame.FriendPicker(flags, UiPickerStatus.List, PickerRows(list, mine, InvitedInto(mine.WorldId)));
    }

    private async Task<UiFrame.FriendPicker?> RefreshedPickerAsync(UiFrame.MyWorld mine, CancellationToken cancellation)
    {
        try { return await PickerAsync(mine, 0, cancellation).ConfigureAwait(false); }
        catch (OperationCanceledException) { return null; }
    }

    // the friend accounts this launcher's invite into world is still pending for (lapsed ones are forgotten)
    private HashSet<string> InvitedInto(string world)
    {
        var now = Now;
        lock (_invitesSent)
        {
            foreach (var gone in _invitesSent.Where(e => e.Value.Until <= now).Select(e => e.Key).ToList()) _invitesSent.Remove(gone);
            return _invitesSent.Where(e => e.Value.World == world).Select(e => e.Key).ToHashSet(StringComparer.Ordinal);
        }
    }

    private static string Cut(string text, int maxBytes)
    {
        if (Encoding.UTF8.GetByteCount(text) <= maxBytes) return text;
        var cut = new StringBuilder();
        var used = 0;
        foreach (var rune in text.EnumerateRunes())
        {
            if (used + rune.Utf8SequenceLength > maxBytes) break;
            used += rune.Utf8SequenceLength;
            cut.Append(rune.ToString());
        }
        return cut.ToString();
    }
}

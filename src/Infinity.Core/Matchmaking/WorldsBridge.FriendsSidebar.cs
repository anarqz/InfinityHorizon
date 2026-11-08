// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Net;
using Infinity.Core.Api;
using Infinity.Core.Online;
using Infinity.Core.Overlay;
using Infinity.Core.Process;

namespace Infinity.Core.Matchmaking;

/// <summary>Core's friend calls the overlay's FRIENDS sidebar makes (CORE_API.md §4.1, §4.2), with the launcher session's token.</summary>
public interface IFriendsSidebarApi
{
    /// <summary><c>GET /launcher/v1/friends</c>.</summary>
    Task<FriendList> FriendsAsync(CancellationToken cancellation);

    /// <summary><c>POST /launcher/v1/friends {game, gamertag}</c>: a request to the account of that profile name, or the acceptance of its request.</summary>
    Task<FriendAddResult> AddByNameAsync(string game, string gamertag, CancellationToken cancellation);

    /// <summary><c>POST /launcher/v1/friends/{friendId}/accept</c>.</summary>
    Task AcceptAsync(string friendId, string idempotencyKey, CancellationToken cancellation);

    /// <summary><c>DELETE /launcher/v1/friends/{friendId}</c>: unfriends, cancels this account's request or declines theirs.</summary>
    Task RemoveAsync(string friendId, string idempotencyKey, CancellationToken cancellation);

    /// <summary><c>POST /launcher/v1/invites/{id}/decline</c> (either kind): the leader declines a request to join.</summary>
    Task DeclineOfferAsync(string offerId, CancellationToken cancellation);
}

/// <summary><see cref="IFriendsSidebarApi"/> over <see cref="ApiClient"/>.</summary>
public sealed class ApiFriendsSidebar(ApiClient api, Func<CancellationToken, Task<string>> access) : IFriendsSidebarApi
{
    private readonly ApiClient _api = api ?? throw new ArgumentNullException(nameof(api));
    private readonly Func<CancellationToken, Task<string>> _access = access ?? throw new ArgumentNullException(nameof(access));

    public async Task<FriendList> FriendsAsync(CancellationToken cancellation) =>
        await _api.FriendsAsync(await _access(cancellation).ConfigureAwait(false), null, cancellation).ConfigureAwait(false);

    public async Task<FriendAddResult> AddByNameAsync(string game, string gamertag, CancellationToken cancellation) =>
        await _api.AddFriendAsync(await _access(cancellation).ConfigureAwait(false), game, gamertag, cancellation).ConfigureAwait(false);

    public async Task AcceptAsync(string friendId, string idempotencyKey, CancellationToken cancellation) =>
        await _api.AcceptFriendAsync(await _access(cancellation).ConfigureAwait(false), friendId, idempotencyKey, cancellation).ConfigureAwait(false);

    public async Task RemoveAsync(string friendId, string idempotencyKey, CancellationToken cancellation) =>
        await _api.RemoveFriendAsync(await _access(cancellation).ConfigureAwait(false), friendId, idempotencyKey, cancellation).ConfigureAwait(false);

    public async Task DeclineOfferAsync(string offerId, CancellationToken cancellation) =>
        await _api.DeclineInviteAsync(await _access(cancellation).ConfigureAwait(false), offerId, cancellation).ConfigureAwait(false);
}

// W15 (OD-211, CR-S102, CORE_API §4.2a): the overlay's right FRIENDS sidebar. The overlay holds no tokens: it shows the last
// FriendsState it got (0x2A) and asks through FriendsAction (0x2B); every answer is a FriendsResult (0x2C) and a fresh state.
// - The list is GET /launcher/v1/friends, read when the sidebar opens (Refresh) and after every action, at most every FriendsFreshFor.
// - Online first; for an online friend what it does per game, from playing.doing (CORE_API §4.2a) and, on an older server, from
//   playing.officialActivity. Every field the server does not send yet (discord, accountSince, profileUrl, doing, online[]) reads as
//   absent; the real fields drive the UI the day the server sends them.
// - Ask to join: POST /mm/v1/worlds/{worldId}/requests (today the server answers 409 {reason: "official"}: a clean refusal). The leader
//   sees the requests in the invites poll (kind: request) and accepts (POST …/requests/{id}/accept) or declines.
// - Invite to my game: the private lobby's invite (InviteSendAsync), only for the lobby's leader.
// - Online toasts come from the invites poll (InviteList.Online): none on the first poll, at most 3 per poll, none while racing.
public sealed partial class WorldsBridge
{
    /// <summary>How long a read friend list answers the sidebar without another read.</summary>
    public static readonly TimeSpan FriendsFreshFor = TimeSpan.FromSeconds(15);

    /// <summary>The most online toasts one poll shows (CORE_API §4.2a).</summary>
    public const int MaxOnlineToasts = 3;

    /// <summary>The sidebar's friend calls (null: no sidebar in this session; actions answer "failed").</summary>
    public IFriendsSidebarApi? FriendsApi { get; init; }

    /// <summary>The game of this play session (<see cref="GameDefinition.Id"/>): "Add friend" names a profile of it.</summary>
    public string SessionGame { get; set; } = InviteGame;

    /// <summary>True while this player races (no online toast then, CORE_API §4.2a). Default <see cref="RacingNow"/>; tests replace it.</summary>
    public Func<bool> IsRacing
    {
        get => _isRacing ?? RacingNow;
        set => _isRacing = value;
    }

    /// <summary><see cref="IsRacing"/> as it is now (INVITE_UX: the invite inbox holds new invite toasts back while it is true).</summary>
    public bool IsRacingNow() => IsRacing();

    /// <summary>The friends' Discord avatars (null: initials only); the same cache as the launcher's sidebar.</summary>
    public Shell.FriendAvatars? Avatars { get; init; }

    private Func<bool>? _isRacing;
    private int _screen = -1; // the hook's last rich-presence screen id (Event 9), -1 none reported
    private readonly object _friendsLock = new();
    private FriendList? _friendsList;
    private long _friendsReadAt = long.MinValue;
    private readonly SemaphoreSlim _friendsRead = new(1, 1);
    private IReadOnlyList<(string Id, string WorldId, string Name)> _joinRequests = [];
    private bool _invitesPolled;
    private readonly HashSet<string> _invitesSeen = new(StringComparer.Ordinal); // the invite ids whose inviter's avatar went ahead
    private string _lastFriendsKey = string.Empty;

    // ------------------------------------------------------------------ the state

    /// <summary>The short label of a game id ("fh2" → "FH2"); an unknown id is shown upper-cased and cut.</summary>
    public static string GameLabel(string? game) =>
        string.IsNullOrEmpty(game) ? string.Empty : Clip(GameDefinition.ById(game)?.ShortName ?? game.ToUpperInvariant(), UiFrame.MaxFriendGame);

    /// <summary>
    /// The sidebar's state from <paramref name="list"/> (null: not read, so "could not load"). <paramref name="haveLobby"/>: this player
    /// leads a private lobby, whose "Who can join" is <paramref name="joinModes"/> (a friend with no profile that fits is excluded).
    /// INVITE_UX (protocol 12): with <paramref name="mine"/> (that lobby) a friend already in it is marked in this lobby, and one in
    /// <paramref name="invited"/> (this launcher's pending invites into it, by friend account) invited; neither may be invited again.
    /// </summary>
    public static UiFrame.FriendsState FriendsStateOf(FriendList? list, bool haveLobby, UiFrame.LobbyJoinModes joinModes,
        IEnumerable<(string Id, string Name)> joinRequests, DateTimeOffset now, UiFrame.MyWorld? mine = null, IReadOnlyCollection<string>? invited = null)
    {
        ArgumentNullException.ThrowIfNull(joinRequests);
        var asks = joinRequests.Where(r => ValidId(r.Id) && r.Name.Length > 0).Take(UiFrame.MaxJoinRequests)
            .Select(r => new FriendRequestRow(r.Id, Clip(r.Name, UiFrame.MaxFriendName))).Where(r => r.Name.Length > 0).ToList();
        if (list is null) return new UiFrame.FriendsState((byte)(haveLobby ? UiFriendsFlags.HaveLobby : 0), [], [], asks);
        var rows = list.Friends.Select(f => RowOf(f, haveLobby, joinModes, now, haveLobby ? mine : null, invited)).Where(r => r is not null).Select(r => r!)
            .OrderBy(r => (r.Flags & UiFriendFlag.Online) != 0 ? 0 : 1).Take(UiFrame.MaxFriendRows).ToList(); // stable: the server's presence order
        var requests = list.Incoming.Select(r => new FriendRequestRow(r.Id, Clip(FirstName(r.Profiles), UiFrame.MaxFriendName)))
            .Where(r => ValidId(r.Id) && r.Name.Length > 0).Take(UiFrame.MaxFriendRequests).ToList();
        return new UiFrame.FriendsState((byte)(UiFriendsFlags.Loaded | (haveLobby ? UiFriendsFlags.HaveLobby : 0)), rows, requests, asks);
    }

    private static string FirstName(IReadOnlyList<FriendProfile> profiles) => profiles.Count > 0 ? profiles[0].Gamertag : string.Empty;

    private static bool ValidId(string id) => id.Length is > 0 and <= UiFrame.MaxPaneId
        && id.All(c => char.IsAsciiLetterOrDigit(c) || c is ':' or '.' or '_' or '-');

    private static FriendRow? RowOf(Friend friend, bool haveLobby, UiFrame.LobbyJoinModes joinModes, DateTimeOffset now, UiFrame.MyWorld? mine,
        IReadOnlyCollection<string>? invited)
    {
        if (!ValidId(friend.Id)) return null;
        var playing = friend.Playing;
        var online = friend.Online;
        var shown = online && playing is { Gamertag.Length: > 0 } ? playing.Gamertag : FirstName(friend.Profiles) is { Length: > 0 } first ? first : playing?.Gamertag;
        var name = Clip(shown, UiFrame.MaxFriendName);
        if (name.Length == 0) return null;
        var activity = UiFriendActivity.Of(playing, online);
        var inWorld = online && playing is { State: "in_world", WorldId.Length: > 0 };
        ushort flags = 0;
        if (online) flags |= UiFriendFlag.Online;
        if (friend.Favorite) flags |= UiFriendFlag.Favorite;
        if (friend.Tier == "vip") flags |= UiFriendFlag.Supporter;
        if (activity == UiFriendActivity.Race) flags |= UiFriendFlag.Racing;
        if (playing?.Doing?.Private == true) flags |= UiFriendFlag.Private;
        var excluded = UiFriendExcluded.None;
        if (haveLobby && online && !FitsLobby(friend, joinModes, playing)) excluded = UiFriendExcluded.Mode;
        if (excluded != UiFriendExcluded.None) flags |= UiFriendFlag.Excluded;
        if (inWorld) flags |= UiFriendFlag.CanAsk;
        var inLobby = mine is { WorldId.Length: > 0 } lobby && (friend.Profiles.Any(p => lobby.Members.Any(m => m.Xuid == p.Xuid))
            || playing is { State: "in_world" } && playing.WorldId == lobby.WorldId);
        var pending = !inLobby && mine is not null && invited?.Contains(friend.Id) == true;
        if (inLobby) flags |= UiFriendFlag.InLobby;
        if (pending) flags |= UiFriendFlag.Invited;
        if (haveLobby && online && excluded == UiFriendExcluded.None && !inLobby && !pending) flags |= UiFriendFlag.CanInvite;
        var days = friend.AccountSince is { } since && DateTimeOffset.TryParse(since, CultureInfo.InvariantCulture,
            DateTimeStyles.AssumeUniversal | DateTimeStyles.AdjustToUniversal, out var created)
            ? (uint)Math.Clamp((now - created).TotalDays, 0, 365 * 100) : FriendRow.Unknown;
        var profiles = friend.Profiles.Select(p => new FriendProfileRow(Clip(p.Gamertag, UiFrame.MaxFriendName), (byte)(p.Mode == "hardcore" ? 0 : 1), GameLabel(p.Game)))
            .Where(p => p.Name.Length > 0).Take(UiFrame.MaxFriendProfiles).ToList();
        var url = (friend.Profiles.FirstOrDefault(p => p.Gamertag == shown && p.ProfileUrl is not null) ?? friend.Profiles.FirstOrDefault(p => p.ProfileUrl is not null))?.ProfileUrl ?? string.Empty;
        if (!url.StartsWith("https://", StringComparison.Ordinal) || System.Text.Encoding.UTF8.GetByteCount(url) > UiFrame.MaxFriendUrl) url = string.Empty;
        return new FriendRow(friend.Id, name, flags, online ? GameLabel(playing?.Game) : string.Empty, activity, excluded,
            Clip(friend.Discord?.Username, UiFrame.MaxFriendName), Clip(friend.Discord?.GlobalName, UiFrame.MaxFriendName), days, url, profiles);
    }

    // "Who can join" (CR-S101): the friend fits when one of its profiles has the mode the lobby takes (any, for Everyone)
    private static bool FitsLobby(Friend friend, UiFrame.LobbyJoinModes joinModes, FriendPlaying? playing)
    {
        if (joinModes.WorldId.Length == 0 || joinModes.Modes == UiJoinModes.Everyone) return true;
        var wanted = UiJoinModes.ToServer(joinModes.Modes);
        return friend.Profiles.Any(p => p.Mode == wanted) || playing?.Mode == wanted;
    }

    /// <summary>The state as the last list, the lobby and the pending join requests say (no request is made).</summary>
    public UiFrame.FriendsState FriendsStateFrame()
    {
        FriendList? list;
        IReadOnlyList<(string Id, string WorldId, string Name)> joins;
        lock (_friendsLock)
        {
            list = _friendsList;
            joins = _joinRequests;
        }
        var mine = Volatile.Read(ref _mineSnapshot);
        var haveLobby = InPrivateMine(mine) && PrivateLobbyPolicy.MayInvite(mine.Leader, _ownXuid);
        return FriendsStateOf(list, haveLobby, JoinModesFrame(), joins.Select(j => (j.Id, j.Name)), Now, mine, haveLobby ? InvitedInto(mine.WorldId) : null);
    }

    // ------------------------------------------------------------------ racing (the toast rule)

    /// <summary>
    /// CORE_API §4.2a's toast rule, "none while the player is in a race", from what this launcher sees itself (the server's
    /// <c>doing.racing</c> is about friends, never about the caller): the hook's last screen (Event 9) is one of the session game's
    /// <see cref="Process.GameDefinition.RaceScreens"/> (FH2: 4-8, 18), or the seat's world runs activity <c>race</c> (FM5's official race
    /// lobby) while the game plays there.
    /// </summary>
    public bool RacingNow()
    {
        var screen = Volatile.Read(ref _screen);
        if (screen >= 0 && Process.GameDefinition.ById(SessionGame)?.RaceScreens.Contains(screen) == true) return true;
        string? activity;
        lock (_mineState) activity = _seatActivity is { } seen && seen.WorldId == Volatile.Read(ref _seatWorld) ? seen.Activity : null;
        return activity == WorldActivities.Race && Volatile.Read(ref _gameInWorld) != 0;
    }

    /// <summary>
    /// OD-223 (T-1108-M): the game may show and take an invite now: its <see cref="Process.GameDefinition.InviteAcceptScreens"/> is empty
    /// (FH2), or the hook's last screen (Event 9) is one of them. No screen reported yet is none of them.
    /// </summary>
    public bool InviteScreenAllows()
    {
        var screens = Process.GameDefinition.ById(SessionGame)?.InviteAcceptScreens;
        if (screens is not { Count: > 0 }) return true;
        var screen = Volatile.Read(ref _screen);
        return screen >= 0 && screens.Contains(screen);
    }

    /// <summary>The hook's Event 9: the game's rich-presence screen id (decimal 0..255), sent when it changes. Anything else is ignored.</summary>
    public void ObserveScreen(string detail)
    {
        var screen = detail is { Length: > 0 and <= 3 } && detail.All(char.IsAsciiDigit) ? int.Parse(detail, CultureInfo.InvariantCulture) : -1;
        if (screen is < 0 or > 255)
        {
            _log("friends: a malformed screen report was ignored");
            return;
        }
        Volatile.Write(ref _screen, screen);
    }

    // ------------------------------------------------------------------ avatars (0x2D)

    private readonly Dictionary<string, string> _avatarsSent = new(StringComparer.Ordinal); // friend id -> the avatar URL the overlay has
    private readonly SemaphoreSlim _avatarSending = new(1, 1);
    private Func<UiFrame, CancellationToken, Task>? _avatarSend;
    private int _avatarsHooked;

    /// <summary>A new overlay said hello: it has no friend avatar yet.</summary>
    public void FriendsOverlayHello()
    {
        lock (_friendsLock) _avatarsSent.Clear();
    }

    /// <summary>
    /// After a <see cref="UiFrame.FriendsState"/>: each listed friend's Discord avatar the overlay lacks (0x2D, from the cache), a clear for
    /// one whose link went, and a load for each not cached yet (sent when it arrives). Nothing is fetched twice (<see cref="Shell.FriendAvatars"/>).
    /// </summary>
    public async Task SendFriendAvatarsAsync(Func<UiFrame, CancellationToken, Task> send, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(send);
        if (Avatars is not { } avatars) return;
        Volatile.Write(ref _avatarSend, send);
        if (Interlocked.Exchange(ref _avatarsHooked, 1) == 0) avatars.Loaded += loaded => { _ = AvatarArrivedAsync(); };
        await _avatarSending.WaitAsync(cancellation).ConfigureAwait(false);
        try
        {
            var frames = new List<UiFrame.FriendAvatar>();
            lock (_friendsLock)
            {
                var wanted = (_friendsList?.Friends ?? []).Where(f => ValidId(f.Id)).Take(UiFrame.MaxFriendRows)
                    .ToDictionary(f => f.Id, f => Shell.FriendAvatars.Allowed(f.Discord?.AvatarUrl) ? f.Discord!.AvatarUrl : null, StringComparer.Ordinal);
                foreach (var (id, url) in wanted)
                {
                    if (url is null)
                    {
                        if (_avatarsSent.Remove(id)) frames.Add(UiFrame.FriendAvatar.Clear(id));
                        continue;
                    }
                    if (_avatarsSent.TryGetValue(id, out var sent) && sent == url) continue;
                    if (avatars.Get(url) is { } rgba)
                    {
                        frames.Add(new UiFrame.FriendAvatar(id, UiFrame.AvatarSide, rgba));
                        _avatarsSent[id] = url;
                    }
                    else _ = avatars.Want(url);
                }
                foreach (var gone in _avatarsSent.Keys.Where(id => !wanted.ContainsKey(id)).ToList()) _avatarsSent.Remove(gone);
            }
            foreach (var frame in frames) await send(frame, cancellation).ConfigureAwait(false);
        }
        finally
        {
            _avatarSending.Release();
        }
    }

    private async Task AvatarArrivedAsync()
    {
        if (Volatile.Read(ref _avatarSend) is not { } send) return;
        try { await SendFriendAvatarsAsync(send, CancellationToken.None).ConfigureAwait(false); }
        catch (Exception e) when (e is ProtocolException or IOException or InvalidOperationException or ObjectDisposedException or OperationCanceledException)
        {
            _log($"friends: an avatar was not sent ({e.GetType().Name})");
        }
    }

    // ------------------------------------------------------------------ the requester's side of an accepted request (CR-S102)

    // the request this player sent last (its id, the friend's world, the friend's name, the world's activity), until the seat read says
    // the leader accepted it; then it is offered once as an invite prompt by the next invites poll
    private sealed record AskedRequest(string Id, string WorldId, string Name, string? Activity);
    private AskedRequest? _askedRequest, _acceptedRequest;

    /// <summary>The seat read's <c>request</c> (MATCHMAKING_API §3.4): this player's request was accepted by the leader.</summary>
    private void ObserveRequestAccepted(SeatRequest request)
    {
        if (Volatile.Read(ref _askedRequest) is { } asked && asked.Id == request.Id && asked.WorldId == request.WorldId
            && Interlocked.CompareExchange(ref _askedRequest, null, asked) == asked)
        {
            Volatile.Write(ref _acceptedRequest, asked);
            _log("friends: the leader accepted this player's request to join; it is offered as an invite");
        }
    }

    // ------------------------------------------------------------------ reads

    private async Task<bool> ReadFriendsAsync(bool force, CancellationToken cancellation)
    {
        if (FriendsApi is not { } api) return false;
        await _friendsRead.WaitAsync(cancellation).ConfigureAwait(false);
        try
        {
            lock (_friendsLock)
            {
                if (!force && _friendsList is not null && _clock.GetElapsedTime(_friendsReadAt) < FriendsFreshFor) return true;
            }
            try
            {
                var list = await api.FriendsAsync(cancellation).ConfigureAwait(false);
                lock (_friendsLock)
                {
                    _friendsList = list;
                    _friendsReadAt = _clock.GetTimestamp();
                }
                return true;
            }
            catch (ApiException e)
            {
                _log($"friends: the list was not read (HTTP {(int)e.Status} {e.Code})");
                return false;
            }
            catch (Exception e) when (HostFailure(e) || e is System.Text.Json.JsonException or NotSupportedException)
            {
                _log($"friends: the list was not read ({e.GetType().Name})");
                return false;
            }
        }
        finally
        {
            _friendsRead.Release();
        }
    }

    // ------------------------------------------------------------------ the invites poll

    /// <summary>
    /// One answer of <c>GET /launcher/v1/invites</c> (called by <see cref="InviteInbox"/> before it applies the list): the requests to join
    /// this player's lobby (<c>kind: request</c>) go to the sidebar, and each friend who came online (<c>online[]</c>) is one toast
    /// (none on the first poll, at most <see cref="MaxOnlineToasts"/>, none while racing) with the list read again.
    /// </summary>
    public async Task OnInvitesPolledAsync(InviteList list, Func<UiFrame, CancellationToken, Task> send, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(list);
        ArgumentNullException.ThrowIfNull(send);
        ObserveSentInvites(list.Sent); // CR-S106: the server's own list of what this account sent
        var requests = list.Items.Where(i => i.Kind == OfferKinds.Request && ValidId(i.Id) && i.World.Id.Length > 0)
            .Select(i => (i.Id, WorldId: i.World.Id, Name: Clip(i.From.Gamertag, UiFrame.MaxFriendName))).Where(r => r.Name.Length > 0)
            .Take(UiFrame.MaxJoinRequests).ToList();
        var first = !_invitesPolled;
        _invitesPolled = true;
        bool changed;
        lock (_friendsLock)
        {
            changed = !_joinRequests.Select(r => r.Id).SequenceEqual(requests.Select(r => r.Id), StringComparer.Ordinal);
            _joinRequests = requests;
        }
        // INVITE_UX: a new invite's toast shows its inviter's avatar, which the overlay has only with a FriendsState (and its 0x2D) sent
        // before it: the list (read if it never was) goes out ahead of the prompt (the inbox applies the list after this observer)
        var fresh = false;
        foreach (var item in list.Items)
            if (item.Kind == OfferKinds.Invite && ValidId(item.Id) && _invitesSeen.Add(item.Id)) fresh = true; // every new id is remembered
        if (fresh)
        {
            if (_invitesSeen.Count > 256) _invitesSeen.Clear();
            bool have;
            lock (_friendsLock) have = _friendsList is not null;
            if (have || await ReadFriendsAsync(force: false, cancellation).ConfigureAwait(false)) changed = true;
        }
        IReadOnlyList<FriendOnline> online = first || list.Online is not { Count: > 0 } ? Array.Empty<FriendOnline>() : list.Online.Take(MaxOnlineToasts).ToList();
        if (online.Count > 0)
        {
            if (!IsRacing())
            {
                foreach (var friend in online)
                {
                    if (Clip(friend.Gamertag, UiFrame.MaxFriendName) is not { Length: > 0 } tag) continue;
                    var title = _text("friends.online").Replace("{gamertag}", tag, StringComparison.Ordinal).Replace("{game}", GameLabel(friend.Game), StringComparison.Ordinal);
                    var body = _text(friend.Mode == "hardcore" ? "friends.mode_hardcore" : "friends.mode_noob");
                    await send(new UiFrame.Toast(UiLevel.Info, 6, 50, Clip(title + "\n" + body, 512)), cancellation).ConfigureAwait(false);
                }
                _log($"friends: {online.Count} friend(s) came online (toasts)");
            }
            if (await ReadFriendsAsync(force: true, cancellation).ConfigureAwait(false)) changed = true;
        }
        if (changed) await SendFriendsStateAsync(send, cancellation).ConfigureAwait(false);
        // CR-S102: the leader let this player in (the seat read saw it): it shows as an invite, then the invite's own accept flow
        if (Interlocked.Exchange(ref _acceptedRequest, null) is { } accepted)
        {
            if (Inbox is { } inbox) await inbox.OfferAcceptedRequestAsync(accepted.Id, accepted.WorldId, accepted.Name, accepted.Activity, send, cancellation).ConfigureAwait(false);
            else _log("friends: an accepted request has no inbox in this session to show it");
        }
    }

    private async Task SendFriendsStateAsync(Func<UiFrame, CancellationToken, Task> send, CancellationToken cancellation)
    {
        var state = FriendsStateFrame();
        try
        {
            await send(state, cancellation).ConfigureAwait(false);
            await SendFriendAvatarsAsync(send, cancellation).ConfigureAwait(false);
        }
        catch (ProtocolException error)
        {
            _log($"friends: the state did not fit the pipe ({error.Message})");
        }
    }

    // ------------------------------------------------------------------ actions

    /// <summary>
    /// One action of the sidebar (0x2B): its result (null: nothing to say, as a Refresh) and, after it, <see cref="FriendsStateFrame"/>
    /// with the list read again where the action changed it.
    /// </summary>
    public async Task<UiFrame.FriendsResult?> FriendsActAsync(UiFrame.FriendsAction action, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(action);
        if (FriendsApi is null) return Result(action, UiFriendsResult.Failed, "friends.failed");
        try
        {
            switch (action.Kind)
            {
                case UiFriendsAction.Refresh:
                    await ReadFriendsAsync(force: false, cancellation).ConfigureAwait(false);
                    return null;
                case UiFriendsAction.Add:
                    return await AddAsync(action, cancellation).ConfigureAwait(false);
                case UiFriendsAction.Accept:
                case UiFriendsAction.Decline:
                case UiFriendsAction.Unfriend:
                    return await FriendCallAsync(action, cancellation).ConfigureAwait(false);
                case UiFriendsAction.AskToJoin:
                    return await AskAsync(action, cancellation).ConfigureAwait(false);
                case UiFriendsAction.InviteToMine:
                    return await InviteAsync(action, cancellation).ConfigureAwait(false);
                case UiFriendsAction.AcceptJoin:
                case UiFriendsAction.DeclineJoin:
                    return await JoinRequestAsync(action, cancellation).ConfigureAwait(false);
                default:
                    return Result(action, UiFriendsResult.Failed, "friends.failed");
            }
        }
        catch (Exception e) when (!cancellation.IsCancellationRequested && (HostFailure(e) || e is System.Text.Json.JsonException or NotSupportedException))
        {
            _log($"friends: action {action.Kind} failed ({e.GetType().Name})");
            return Result(action, UiFriendsResult.Failed, "friends.failed");
        }
    }

    private UiFrame.FriendsResult Result(UiFrame.FriendsAction action, byte result, string key, string? name = null) =>
        new(action.Kind, result, Clip((name is null ? _text(key) : _text(key).Replace("{name}", name, StringComparison.Ordinal)), UiFrame.MaxFriendMessage));

    private async Task<UiFrame.FriendsResult> AddAsync(UiFrame.FriendsAction action, CancellationToken cancellation)
    {
        var name = Clip(action.Text.Trim(), UiFrame.MaxFriendName);
        try
        {
            var added = await FriendsApi!.AddByNameAsync(SessionGame, name, cancellation).ConfigureAwait(false);
            await ReadFriendsAsync(force: true, cancellation).ConfigureAwait(false);
            _log($"friends: add answered {added.State}");
            return added.State == "friends"
                ? Result(action, UiFriendsResult.NowFriends, "friends.added_friends", name)
                : Result(action, UiFriendsResult.Requested, "friends.added_requested", name);
        }
        catch (ApiException e)
        {
            _log($"friends: add refused (HTTP {(int)e.Status} {e.Code})");
            return PlayerActions.FriendRefusal(e) switch
            {
                UiPlayerResult.PlayerNotFound => Result(action, UiFriendsResult.NotFound, "friends.not_found", name),
                UiPlayerResult.FriendLimit => Result(action, UiFriendsResult.Refused, "friends.limit"),
                UiPlayerResult.TryLater => Result(action, UiFriendsResult.Refused, "friends.try_later"),
                UiPlayerResult.None => Result(action, UiFriendsResult.Refused, "friends.failed"), // one's own profile
                _ => Result(action, UiFriendsResult.Failed, "friends.failed"),
            };
        }
    }

    private async Task<UiFrame.FriendsResult> FriendCallAsync(UiFrame.FriendsAction action, CancellationToken cancellation)
    {
        var name = FriendNameOf(action.Id);
        var key = Guid.CreateVersion7().ToString();
        try
        {
            if (action.Kind == UiFriendsAction.Accept) await FriendsApi!.AcceptAsync(action.Id, key, cancellation).ConfigureAwait(false);
            else await FriendsApi!.RemoveAsync(action.Id, key, cancellation).ConfigureAwait(false);
        }
        catch (ApiException e)
        {
            _log($"friends: action {action.Kind} refused (HTTP {(int)e.Status} {e.Code})");
            return Result(action, e.Code == "PORTAL.NOT_FOUND" ? UiFriendsResult.NotFound : UiFriendsResult.Failed,
                PlayerActions.FriendRefusal(e) == UiPlayerResult.TryLater ? "friends.try_later" : "friends.failed");
        }
        await ReadFriendsAsync(force: true, cancellation).ConfigureAwait(false);
        return action.Kind switch
        {
            UiFriendsAction.Accept => Result(action, UiFriendsResult.NowFriends, "friends.added_friends", name),
            UiFriendsAction.Decline => Result(action, UiFriendsResult.Done, "friends.declined"),
            _ => Result(action, UiFriendsResult.Done, "friends.unfriended", name),
        };
    }

    private string FriendNameOf(string id)
    {
        lock (_friendsLock)
        {
            var list = _friendsList;
            var shown = list?.Friends.FirstOrDefault(f => f.Id == id) is { } friend ? friend.Playing?.Gamertag ?? FirstName(friend.Profiles)
                : list?.Incoming.FirstOrDefault(r => r.Id == id) is { } request ? FirstName(request.Profiles) : null;
            return Clip(shown, UiFrame.MaxFriendName);
        }
    }

    private async Task<UiFrame.FriendsResult> AskAsync(UiFrame.FriendsAction action, CancellationToken cancellation)
    {
        Friend? friend;
        lock (_friendsLock) friend = _friendsList?.Friends.FirstOrDefault(f => f.Id == action.Id);
        var name = Clip(friend?.Playing?.Gamertag ?? (friend is null ? null : FirstName(friend.Profiles)), UiFrame.MaxFriendName);
        if (friend is not { Online: true, Playing: { State: "in_world", WorldId.Length: > 0 } playing })
            return Result(action, UiFriendsResult.Refused, "friends.ask_not_in_world", name);
        var (mods, refusal) = WireMods();
        if (refusal is not null) return new UiFrame.FriendsResult(action.Kind, UiFriendsResult.Refused, Clip(_text(refusal), UiFrame.MaxFriendMessage));
        var answer = await _mm.RequestAsync(playing.WorldId!, mods, cancellation).ConfigureAwait(false);
        if (answer.Ok)
        {
            // CR-S102: remembered until the seat read says the leader accepted it (then it shows as an invite)
            if (answer.Value is { Id.Length: > 0 } sent)
                Volatile.Write(ref _askedRequest, new AskedRequest(sent.Id, playing.WorldId!, name, playing.Doing?.Activity ?? playing.OfficialActivity));
            _log("friends: asked to join a friend's world");
            return Result(action, UiFriendsResult.Done, "friends.ask_sent", name);
        }
        var error = answer.Error!;
        _log($"friends: ask to join refused ({(int)error.Status} {error.Kind} {error.Reason})");
        return Result(action, UiFriendsResult.Refused, AskRefusalKey(error), name);
    }

    /// <summary>
    /// The words of a refused "Ask to join" (<c>POST /mm/v1/worlds/{id}/requests</c>, MATCHMAKING_API §2.10 and §3.4, CR-S102), a
    /// <c>friends.*</c> key ({name}: the friend).
    /// </summary>
    public static string AskRefusalKey(MmError error)
    {
        ArgumentNullException.ThrowIfNull(error);
        return error.Kind switch
        {
            MmErrorKind.Conflict => error.Reason switch
            {
                ConflictReasons.Official => "friends.ask_unavailable", // a public official world takes no requests
                ConflictReasons.NotLeader => "friends.ask_no_leader",
                ConflictReasons.OwnWorld => "friends.ask_own",
                ConflictReasons.Seated => "friends.ask_seated",
                ConflictReasons.Full or ConflictReasons.PendingLimit => "friends.ask_full",
                ConflictReasons.Pool => "friends.ask_pool",
                ConflictReasons.ClientUpdate => "friends.ask_update",
                _ => "friends.ask_failed",
            },
            MmErrorKind.NotFound => "friends.ask_friends_only", // PORTAL.NOT_FOUND: not a friend of the leader's account
            MmErrorKind.WorldNotFound => "friends.ask_not_in_world", // closed, another mode's, or kicked from it
            MmErrorKind.NotHost => "friends.ask_friends_only",
            MmErrorKind.RateLimited => "friends.try_later",
            _ => "friends.ask_failed",
        };
    }

    /// <summary>The words of a refused accept of a request to join by the leader (<c>POST …/requests/{rid}/accept</c>), a <c>friends.*</c> key.</summary>
    public static string AcceptRefusalKey(MmError error)
    {
        ArgumentNullException.ThrowIfNull(error);
        return error.Kind switch
        {
            MmErrorKind.Conflict => error.Reason switch
            {
                ConflictReasons.NotLeader => "friends.join_not_leader",
                ConflictReasons.NotAllowed => "friends.join_not_allowed",
                ConflictReasons.Full => "friends.join_full",
                ConflictReasons.SeatedElsewhere => "friends.join_elsewhere",
                ConflictReasons.Ended => "friends.join_gone",
                ConflictReasons.Pool => "friends.join_pool",
                _ => "friends.join_failed",
            },
            MmErrorKind.NotFound => "friends.join_gone",
            MmErrorKind.NotHost => "friends.join_not_leader",
            MmErrorKind.RateLimited => "friends.try_later",
            _ => "friends.join_failed",
        };
    }

    private async Task<UiFrame.FriendsResult> InviteAsync(UiFrame.FriendsAction action, CancellationToken cancellation)
    {
        Friend? friend;
        lock (_friendsLock) friend = _friendsList?.Friends.FirstOrDefault(f => f.Id == action.Id);
        var name = Clip(friend?.Playing?.Gamertag ?? (friend is null ? null : FirstName(friend.Profiles)), UiFrame.MaxFriendName);
        var mine = Volatile.Read(ref _mineSnapshot);
        if (!InPrivateMine(mine) || !PrivateLobbyPolicy.MayInvite(mine.Leader, _ownXuid))
            return Result(action, UiFriendsResult.Refused, "friends.invite_no_lobby");
        var profile = friend?.Playing is { } now && Decimal(now.Xuid) && now.Game == SessionGame ? now.Xuid
            : friend?.Profiles.FirstOrDefault(p => p.Game == SessionGame && Decimal(p.Xuid))?.Xuid;
        if (friend is null || profile is null) return Result(action, UiFriendsResult.Refused, "friends.invite_excluded", name);
        if (!FitsLobby(friend, JoinModesFrame(), friend.Playing)) return Result(action, UiFriendsResult.Refused, "friends.invite_excluded", name);
        var sent = await InviteSendAsync(profile, cancellation).ConfigureAwait(false);
        return new UiFrame.FriendsResult(action.Kind, sent.Notice.Icon == 1 ? UiFriendsResult.Done : UiFriendsResult.Refused,
            Clip(sent.Notice.Text.Replace('\n', ' '), UiFrame.MaxFriendMessage));
    }

    private async Task<UiFrame.FriendsResult> JoinRequestAsync(UiFrame.FriendsAction action, CancellationToken cancellation)
    {
        (string Id, string WorldId, string Name)? request;
        lock (_friendsLock) request = _joinRequests.Where(r => r.Id == action.Id).Select(r => ((string, string, string)?)r).FirstOrDefault();
        if (request is not { } known) return Result(action, UiFriendsResult.Refused, "friends.join_gone");
        if (action.Kind == UiFriendsAction.DeclineJoin)
        {
            await FriendsApi!.DeclineOfferAsync(known.Id, cancellation).ConfigureAwait(false);
            DropJoinRequest(known.Id);
            return Result(action, UiFriendsResult.Done, "friends.declined");
        }
        var answer = await _mm.AcceptRequestAsync(known.WorldId, known.Id, cancellation).ConfigureAwait(false);
        if (answer.Ok)
        {
            DropJoinRequest(known.Id);
            _log("friends: join request accepted");
            return Result(action, UiFriendsResult.Done, "friends.join_accepted", known.Name);
        }
        var error = answer.Error!;
        _log($"friends: join request refused ({(int)error.Status} {error.Kind} {error.Reason})");
        var key = AcceptRefusalKey(error);
        // a request that can never be accepted here leaves the list at once; seated_elsewhere, full and a rate limit keep it
        if (key is "friends.join_gone" or "friends.join_not_leader" or "friends.join_not_allowed" or "friends.join_pool") DropJoinRequest(known.Id);
        return Result(action, key is "friends.join_failed" or "friends.try_later" ? UiFriendsResult.Failed : UiFriendsResult.Refused, key, known.Name);
    }

    private void DropJoinRequest(string id)
    {
        lock (_friendsLock) _joinRequests = _joinRequests.Where(r => r.Id != id).ToList();
    }
}

// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.IO;
using System.Net.Http;
using Infinity.Core.Api;
using Infinity.Core.Matchmaking;
using Infinity.Core.Overlay;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>A friend of the FRIENDS sidebar: the profile shown, what it does now, and a live invite from it to this account.</summary>
public sealed record FriendCard(string Id, string Name, string Status, bool Online, bool Supporter, string? InviteId, string Profiles)
{
    public string Initial => Name.Length > 0 ? Name[..1].ToUpperInvariant() : "?";
    public bool Invited => InviteId is not null;
    public bool HasProfiles => Profiles.Length > 0;
}

/// <summary>An incoming friend request: its id (accept / decline) and the asker's profile name.</summary>
public sealed record FriendRequestCard(string Id, string Name);

/// <summary>
/// Owner 2026-11-08: the FRIENDS sidebar, at shell level (any page), opened and closed by the header's friends button ("{icon} online/total",
/// green dot for a pending request or invite); closed by default, its state remembered (<see cref="FriendsSidebarStore"/>). It matches the
/// in-game overlay's sidebar (W15) with the same Core client (CORE_API §4.1/§4.2a), no new route: add by gamertag on top, then the friends
/// online first and by name, then incoming requests and invites with accept/decline. The counts and the dot come from
/// <see cref="FriendsPoller"/> (every 30 s while signed in, Retry-After respected), never from a click; opening the sidebar shows the last
/// poll's list. Our own add/accept/decline reads the list once again, inside its busy overlay. Accepting an invite joins a world, which
/// needs the game: the in-game overlay does it (the launcher can decline).
/// </summary>
public sealed partial class ShellViewModel
{
    private string _friendAddName = string.Empty;
    private string? _friendsStatus;
    private FriendList? _friendList;
    private bool? _friendsOpen;
    private CancellationTokenSource? _friendsPoll;

    public ObservableCollection<FriendCard> FriendCards { get; } = [];

    public ObservableCollection<FriendRequestCard> FriendRequestCards { get; } = [];

    /// <summary>The sidebar is open (remembered across launches; closed by default).</summary>
    public bool FriendsOpen => _friendsOpen ??= FriendsSidebarStore.ReadOpen(_stateDirectory);

    public bool ShowFriendsSidebar => ShellLayout.FriendsSidebar(FriendsOpen, SignedIn && _session is not null);

    public bool ShowFriendsButton => ShellLayout.FriendsButton(SignedIn && _session is not null);

    public FriendsSummary FriendsSummary => LauncherFriends.Summary(_friendList);

    /// <summary>"3/12" beside the friends icon.</summary>
    public string FriendsCounts => FriendsSummary.Counts;

    public bool FriendsPending => FriendsSummary.Pending;

    public string FriendsButtonName => S.Format("friends.button", ("online", FriendsSummary.Online.ToString(SavesCulture)), ("total", FriendsSummary.Total.ToString(SavesCulture)));

    public bool HasFriendRequests => FriendRequestCards.Count > 0;

    public bool FriendsEmpty => _friendList is not null && FriendCards.Count == 0;

    public bool FriendsNotRead => _friendList is null;

    public string FriendsStatus => _friendsStatus ?? string.Empty;

    public bool HasFriendsStatus => !string.IsNullOrEmpty(_friendsStatus);

    public string FriendAddName
    {
        get => _friendAddName;
        set { _friendAddName = value ?? string.Empty; Changed(); Changed(nameof(CanAddFriend)); }
    }

    public bool CanAddFriend => Idle && _session is not null && ProfileName.IsValid(_friendAddName.Trim());

    /// <summary>The header's friends button.</summary>
    public void ToggleFriends()
    {
        _friendsOpen = !FriendsOpen;
        try { FriendsSidebarStore.WriteOpen(_stateDirectory, _friendsOpen.Value); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { Log("friends: sidebar state not saved (" + e.GetType().Name + ")"); }
        ChangedFriends();
    }

    /// <summary>Sign-in: the presence poll starts (its first read is at once; no busy overlay, as the other background reads).</summary>
    private void StartFriendsPoll()
    {
        StopFriendsPoll();
        if (_session is not { } session) return;
        var stop = _friendsPoll = new CancellationTokenSource();
        var poller = new FriendsPoller(c => FetchFriendsAsync(session, c), list =>
        {
            if (ReferenceEquals(_session, session) && !stop.IsCancellationRequested) ShowFriends(list);
        }, log: Log);
        _ = poller.RunAsync(stop.Token);
    }

    private void StopFriendsPoll()
    {
        _friendsPoll?.Cancel();
        _friendsPoll?.Dispose();
        _friendsPoll = null;
    }

    private static Task<FriendList> FetchFriendsAsync(Infinity.Core.Auth.AccountSession session, CancellationToken cancellation) =>
        session.Tokens.WithAccessAsync(access => session.Api.FriendsAsync(access, null, cancellation), cancellation);

    private void ShowFriends(FriendList list)
    {
        if (_session is null) return;
        _friendList = list;
        FriendCards.Clear();
        foreach (var friend in LauncherFriends.Sorted(list))
        {
            var name = LauncherFriends.ShownName(friend);
            if (name.Length == 0) continue;
            var profiles = string.Join("  ·  ", friend.Profiles.Select(p => S.Format(p.Mode == "hardcore" ? "friends.profile_hardcore" : "friends.profile_noob",
                ("name", p.Gamertag), ("game", WorldsBridge.GameLabel(p.Game)))));
            FriendCards.Add(new FriendCard(friend.Id, name, FriendStatus(friend), friend.Online, friend.Tier == "vip", friend.Invite?.Id, profiles));
        }
        FriendRequestCards.Clear();
        foreach (var request in list.Incoming)
        {
            var asker = request.Profiles.Count > 0 ? request.Profiles[0].Gamertag : null;
            if (!string.IsNullOrEmpty(asker)) FriendRequestCards.Add(new FriendRequestCard(request.Id, asker));
        }
        ChangedFriends();
    }

    /// <summary>"Offline", "Online", "In FH2", "Free roam on FH2", "Racing on FH2"... (en + pt-BR), as the overlay words it.</summary>
    private string FriendStatus(Friend friend)
    {
        if (!friend.Online) return S["play.friend_offline"];
        var game = WorldsBridge.GameLabel(friend.Playing?.Game);
        if (game.Length == 0) return S["play.friend_online"];
        var key = UiFriendActivity.Of(friend.Playing, friend.Online) switch
        {
            UiFriendActivity.FreeRoam => "play.friend_freeroam",
            UiFriendActivity.CarMeet => "play.friend_carmeet",
            UiFriendActivity.RoadTrip => "play.friend_roadtrip",
            UiFriendActivity.Race => "play.friend_racing",
            _ => "play.friend_in_game",
        };
        var text = S.Format(key, ("game", game));
        return friend.Playing?.Doing?.Private == true ? text + " · " + S["play.friend_private"] : text;
    }

    /// <summary>"Add friend": a profile name of the selected game; friendship is by account (CORE_API §4.1).</summary>
    public async Task AddFriendAsync()
    {
        if (!CanAddFriend || _session is not { } session) return;
        var name = _friendAddName.Trim();
        var game = CurrentGame.Id;
        await Run(S["busy.friends_add"], async () =>
        {
            try
            {
                var added = await session.Tokens.WithAccessAsync(access => session.Api.AddFriendAsync(access, game, name, CancellationToken.None), CancellationToken.None);
                Log($"friends: add answered {added.State}");
                _friendsStatus = S.Format(added.State == "friends" ? "friends.added_friends" : "friends.added_requested", ("name", name));
                FriendAddName = string.Empty;
            }
            catch (ApiException e) when (Infinity.Core.Online.OnlinePause.From(e) is null)
            {
                Log($"friends: add refused (HTTP {(int)e.Status} {e.Code})");
                _friendsStatus = FriendRefusalText(e, name);
                ChangedFriends();
                return;
            }
            await ReloadFriendsAfterWriteAsync(session);
        });
    }

    private string FriendRefusalText(ApiException e, string? name = null) => PlayerActions.FriendRefusal(e) switch
    {
        UiPlayerResult.PlayerNotFound => S.Format("friends.not_found", ("name", name ?? string.Empty)),
        UiPlayerResult.FriendLimit => S["friends.limit"],
        UiPlayerResult.TryLater => RateLimits.Describe(e) is { } rate ? RateText(rate) : S["friends.try_later"],
        _ => S["friends.failed"],
    };

    /// <summary>Accept an incoming friend request.</summary>
    public Task AcceptFriendRequestAsync(FriendRequestCard request) => AnswerFriendRequestAsync(request, accept: true);

    /// <summary>Decline an incoming friend request (<c>DELETE /launcher/v1/friends/{id}</c>).</summary>
    public Task DeclineFriendRequestAsync(FriendRequestCard request) => AnswerFriendRequestAsync(request, accept: false);

    private async Task AnswerFriendRequestAsync(FriendRequestCard request, bool accept)
    {
        ArgumentNullException.ThrowIfNull(request);
        if (_session is not { } session || _busy) return;
        var key = Guid.CreateVersion7().ToString();
        await Run(S[accept ? "busy.friends_accept" : "busy.friends_decline"], async () =>
        {
            try
            {
                if (accept) await session.Tokens.WithAccessAsync(access => session.Api.AcceptFriendAsync(access, request.Id, key, CancellationToken.None), CancellationToken.None);
                else await session.Tokens.WithAccessAsync(async access => { await session.Api.RemoveFriendAsync(access, request.Id, key, CancellationToken.None); return true; }, CancellationToken.None);
                Log("friends: request " + (accept ? "accepted" : "declined"));
                _friendsStatus = accept ? S.Format("friends.added_friends", ("name", request.Name)) : S["friends.declined"];
            }
            catch (ApiException e) when (Infinity.Core.Online.OnlinePause.From(e) is null)
            {
                Log($"friends: request answer refused (HTTP {(int)e.Status} {e.Code})");
                _friendsStatus = FriendRefusalText(e, request.Name);
                ChangedFriends();
                return;
            }
            await ReloadFriendsAfterWriteAsync(session);
        });
    }

    /// <summary>A friend's invite: declined from here (<c>POST /launcher/v1/invites/{id}/decline</c>); accepting joins, in the game's overlay.</summary>
    public async Task DeclineFriendInviteAsync(FriendCard friend)
    {
        ArgumentNullException.ThrowIfNull(friend);
        if (_session is not { } session || _busy || friend.InviteId is not { } invite) return;
        await Run(S["busy.friends_decline"], async () =>
        {
            try
            {
                await session.Tokens.WithAccessAsync(async access => { await session.Api.DeclineInviteAsync(access, invite, CancellationToken.None); return true; }, CancellationToken.None);
                Log("friends: invite declined");
                _friendsStatus = S["friends.declined"];
            }
            catch (ApiException e) when (Infinity.Core.Online.OnlinePause.From(e) is null)
            {
                Log($"friends: invite decline refused (HTTP {(int)e.Status} {e.Code})");
                _friendsStatus = FriendRefusalText(e, friend.Name);
                ChangedFriends();
                return;
            }
            await ReloadFriendsAfterWriteAsync(session);
        });
    }

    /// <summary>Accepting an invite joins the friend's lobby: the game must run, and the overlay does it (never silently nothing).</summary>
    public void AcceptFriendInvite(FriendCard friend)
    {
        ArgumentNullException.ThrowIfNull(friend);
        _friendsStatus = S[Playing ? "play.invite_in_overlay" : "play.invite_needs_game"];
        ChangedFriends();
    }

    /// <summary>Our own write changed the list: read it once again now (inside the same busy overlay).</summary>
    private async Task ReloadFriendsAfterWriteAsync(Infinity.Core.Auth.AccountSession session) =>
        ShowFriends(await FetchFriendsAsync(session, CancellationToken.None));

    private void ClearFriends()
    {
        StopFriendsPoll();
        FriendCards.Clear();
        FriendRequestCards.Clear();
        _friendList = null;
        _friendsStatus = null;
        _friendAddName = string.Empty;
        ChangedFriends();
    }

    private void ChangedFriends()
    {
        foreach (var name in new[] { nameof(HasFriendRequests), nameof(FriendsEmpty), nameof(FriendsNotRead), nameof(FriendsStatus), nameof(HasFriendsStatus),
            nameof(FriendAddName), nameof(CanAddFriend), nameof(FriendsOpen), nameof(ShowFriendsSidebar), nameof(ShowFriendsButton), nameof(FriendsSummary),
            nameof(FriendsCounts), nameof(FriendsPending), nameof(FriendsButtonName) }) Changed(name);
    }
}

// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Api;
using Infinity.Core.Overlay;

namespace Infinity.Core.Matchmaking;

// OD-190 (MATCHMAKING_API §2.9 "Join friend", CORE_API §4.1 FriendPlaying.officialActivity): a friend in an official world is joined
// by that world's id. The server lets the caller in while the friend holds a used seat there, the mode fits (any for car meets) and
// it is not an OD-191 group world; else 200 not_allowed {reason: "friends"} or full, said with the join's own refusal texts.
public sealed partial class WorldsBridge
{
    /// <summary>
    /// Whether "Join friend" is offered for <paramref name="playing"/>: an official world by its <c>officialActivity</c> (the server's
    /// join decides, <c>joinable</c> is not read for it), a player world by <c>joinable</c> (CORE_API §4.1).
    /// </summary>
    public static bool JoinFriendOffered(FriendPlaying? playing) => playing is { State: "in_world", WorldId: { Length: > 0 } }
        && (playing.OfficialActivity is { } activity ? WorldActivities.Placement.Contains(activity) : playing.Joinable == true);

    /// <summary>
    /// "Join friend": <c>POST /mm/v1/join {worldId}</c> through the overlay's own join. A seat held elsewhere answers <c>locked</c>: it is
    /// let go (<c>DELETE /mm/v1/seat</c>) and the join asked once more, never again. A seat granted in a friend's official world is
    /// recorded as official with that activity, so the next lobby ask of that activity stays in it (OD-190 §11.3 item 1).
    /// </summary>
    public async Task<UiFrame.JoinResult> JoinFriendAsync(FriendPlaying friend, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(friend);
        if (!JoinFriendOffered(friend) || friend.WorldId is not { } world) return new(UiJoin.Closed, _text("worlds.closed"));
        var joined = await JoinAsync(new UiFrame.JoinWorld(world, string.Empty), cancellation).ConfigureAwait(false);
        if (joined.Result == UiJoin.Reserved && friend.OfficialActivity is { } activity && Volatile.Read(ref _seatWorld) == world)
        {
            Volatile.Write(ref _seatOfficial, world);
            lock (_mineState) _seatActivity = (world, activity);
            // taken for the lobby ask its activity is, so test.11 never drops it for that ask
            if (_seat.GrantId is { } grant)
                SeatTakenFor(grant, activity switch { WorldActivities.CarMeet => FreeRoamKind.CarMeet, WorldActivities.RoadTrip => FreeRoamKind.RoadTrip, _ => FreeRoamKind.FreeRoam });
            _log($"worlds: joined a friend's official {activity} world {world}");
        }
        return joined;
    }
}

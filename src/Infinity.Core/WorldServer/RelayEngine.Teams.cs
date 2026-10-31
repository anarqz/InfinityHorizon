// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Security.Cryptography;

namespace Infinity.Core.WorldServer;

/// <summary>
/// Team events (NR-10; OD-165 depends on it; SAURON's RE 2026-10-31). An event is a team event when its options say NumTeams != 0
/// (the game's NetworkIsTeamRace; Free Roam "Teams: On"), or in a Road Trip when it is the team twin of an online event (its id +
/// 10000). King and Infected never have teams (their presets lock NumTeams to 0). When the participants are selected, the relay
/// shuffles them and alternates red (0) and blue (1), writes each one's TeamId (player object, PlayerChoices var 90, 1 bit) as a
/// server update (like PrivateUniverseId: no client sets it, the game never resets it) BEFORE the starting grid goes out (the race
/// setup copies it), and the grid keeps the alternation (InterleaveTeams). TeamId 0 is also "no team", so it never says a team
/// event is on: the relay keeps that itself, from selection until the event ends (SessionHost InRace 1 -> 0, EndRace) or a player
/// leaves it (its ParticipatingInRace to 0). An event without teams sets every TeamId written before back to 0. Teams change only
/// between events. Voice follows (MATCHMAKING_API.md §5c rule 2a): each player hears only its hearing group while the event runs.
/// </summary>
internal sealed partial class RelayEngine
{
    /// <summary>LobbyPlayer PlayerChoices TeamId on the player object (8-bit var ids), 1 bit: 0 red, 1 blue.</summary>
    internal const byte TeamVariable = 90;
    internal const int TeamBits = 1;
    internal const byte TeamRed = 0, TeamBlue = 1;
    /// <summary>Every online event exists twice: its id and, as its team variant, id + 10000.</summary>
    internal const int TeamVariantOffset = 10000;

    private bool _teamEvent;
    // participants whose team the game does not have yet (no player object to write it on): they hear nobody until it is written
    private readonly HashSet<byte> _teamPending = [];
    private Random? _teamRandom;

    /// <summary>A team event runs (from its participants' selection to its end).</summary>
    internal bool TeamEventActive => _teamEvent;

    /// <summary>The running team event's teams by XUID (0 red, 1 blue); empty outside one.</summary>
    internal IReadOnlyDictionary<ulong, byte> Teams =>
        _peers.Where(p => p.Team is not null).ToDictionary(p => p.Xuid, p => p.Team!.Value);

    private bool IsTeamEvent()
    {
        var fields = GameOptionsFields.TryRead(_gameOptions.Records, out var read) ? read : null;
        if (fields?.Tag is not null) return false; // King and Infected: never teams
        if (_roadTrip is { Winner: not null } trip && trip.CurrentRace >= TeamVariantOffset) return true;
        return fields?.NumTeams is > 0; // the game's own test (NetworkIsTeamRace): NumTeams != 0
    }

    /// <summary>Participants were selected (SelectParticipants): teams for a team event, TeamId back to 0 otherwise.</summary>
    private void AssignTeams(Race race)
    {
        _teamPending.Clear();
        foreach (var peer in _peers) peer.Team = null;
        if (!IsTeamEvent())
        {
            var wasTeamEvent = _teamEvent;
            _teamEvent = false;
            foreach (var peer in _peers.Where(p => p.Synced && p.TeamWritten is { } written && written != TeamRed)) WriteTeam(peer, TeamRed);
            if (wasTeamEvent) VoiceGroupsChanged();
            return;
        }
        var order = race.Participants.ToList();
        var random = _teamRandom ??= new Random(_settings.TeamSeed ?? RandomNumberGenerator.GetInt32(int.MaxValue));
        for (var i = order.Count - 1; i > 0; i--)
        {
            var j = random.Next(i + 1);
            (order[i], order[j]) = (order[j], order[i]);
        }
        _teamEvent = true;
        for (var i = 0; i < order.Count; i++)
        {
            if (_peers.FirstOrDefault(p => p.Index == order[i]) is not { } peer) continue;
            peer.Team = (byte)(i % 2 == 0 ? TeamRed : TeamBlue);
            if (!WriteTeam(peer, peer.Team.Value)) _teamPending.Add(peer.Index);
        }
        race.Grid = order; // odd grid slots red, even blue
        Note(string.Create(CultureInfo.InvariantCulture,
            $"teams: red {string.Join(",", order.Where((_, i) => i % 2 == 0))}, blue {string.Join(",", order.Where((_, i) => i % 2 == 1))}"));
        VoiceGroupsChanged();
    }

    /// <summary>The server's TeamId for a player: to its owner, and to everybody else on its relayed object. False: no player
    /// object yet (written when it is created).</summary>
    private bool WriteTeam(RelayPeer peer, byte team)
    {
        if (!peer.Synced || !_peers.Contains(peer)) return false;
        var player = peer.Objects.FirstOrDefault(p => p.Key.Router == 1);
        if (player.Value is not { } obj) return false;
        var id = player.Key.Id;
        var payload = new BitBuffer().Push(TeamVariable, 8).Push(team, TeamBits);
        var sequence = peer.PlayerSequence is { } previous ? AckWindow.SequenceNext(previous) : (byte)0;
        peer.PlayerSequence = sequence;
        SendSequenced(peer, [NerdCodec.Write(new NerdMessage.Object(false, 1, new(HandleKind.Server, id), 0, sequence, payload))]);
        obj.Injected = AckWindow.SequenceNext(obj.Injected);
        obj.Team = team;
        Forward(peer, [NerdCodec.Write(new NerdMessage.Object(false, 1, new(HandleKind.Relay, id, peer.Index), 0,
            AckWindow.SequenceAdd(obj.Last, obj.Injected), payload))], true);
        peer.TeamWritten = team;
        return true;
    }

    /// <summary>A player's router-1 object was created (it starts at TeamId 0): the team of the running event reaches it now.</summary>
    private void TeamObjectCreated(RelayPeer peer)
    {
        peer.TeamWritten = null;
        if (peer.Team is not { } team || !WriteTeam(peer, team)) return;
        if (_teamPending.Remove(peer.Index)) VoiceGroupsChanged();
    }

    /// <summary>A participant left the event (RemoveRaceParticipant): it is outside the event for voice; its TeamId stays.</summary>
    private void TeamPlayerLeft(RelayPeer peer)
    {
        if (!_teamEvent || peer.Team is null) return;
        peer.Team = null;
        _teamPending.Remove(peer.Index);
        VoiceGroupsChanged();
    }

    /// <summary>The event ended (EndRace): the lobby's voice applies again; TeamIds stay until the next event's start.</summary>
    private void TeamEventEnded()
    {
        if (!_teamEvent) return;
        _teamEvent = false;
        _teamPending.Clear();
        foreach (var peer in _peers) peer.Team = null;
        Note("teams: the team event ended");
        VoiceGroupsChanged();
    }

    /// <summary>
    /// §5c rule 2a: a player's hearing group. Outside team events everybody shares one (0). In a team event a participant's group is
    /// its team (1 red, 2 blue), the others' "outside the event" (0); a participant whose team the game does not have yet shares a
    /// group with nobody (negative, unique).
    /// </summary>
    private int HearingGroup(RelayPeer peer)
    {
        if (!_teamEvent || _race is not { } race || !race.Participants.Contains(peer.Index)) return 0;
        return peer.Team is { } team && !_teamPending.Contains(peer.Index) ? 1 + team : -1 - peer.Index;
    }

    private bool SameHearingGroup(RelayPeer a, RelayPeer b)
    {
        var group = HearingGroup(a);
        return group >= 0 && group == HearingGroup(b);
    }
}

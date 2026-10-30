// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.WorldServer;

internal sealed partial class RelayEngine
{
    private enum RaceStage { Inviting, Gathering, Grid, Started }
    private sealed class Race(RelayPeer owner, long inviteUntil)
    {
        internal RelayPeer OwnerPeer { get; } = owner;
        internal byte Owner => OwnerPeer.Index;
        internal List<byte> Participants { get; } = [owner.Index];
        internal HashSet<byte> Finished { get; } = [];
        internal HashSet<byte> InProgress { get; } = [];
        internal RaceStage Stage { get; set; }
        internal long InviteUntil { get; } = inviteUntil;
        internal long StageUntil { get; set; }
        internal long? FirstFinishAt { get; set; }
    }
    private byte? _eventOwner;
    private Race? _race;
    private bool IsEventOwner(RelayPeer peer) =>
        _eventOwner == peer.Index && (_race is null
            || ReferenceEquals(_race.OwnerPeer, peer) && _race.Participants.Contains(peer.Index));

    private void SetHostVariables(params (byte Variable, ulong Value, int Width)[] variables)
    {
        foreach (var (index, value, width) in variables) _hostVariables[index] = (value, width);
        _hostSequence = AckWindow.SequenceNext(_hostSequence);
        var message = NerdCodec.Write(new NerdMessage.Object(false, 6, new(HandleKind.Own, 0), 0,
            _hostSequence, NerdPayload.SessionHost(variables)));
        foreach (var peer in _peers.Where(p => p.Synced)) SendSequenced(peer, [message]);
    }

    private void SetConsoleVariables(RelayPeer peer, params (byte Variable, bool Value)[] variables)
    {
        // These flags are authoritative even before a client has created its console object.
        foreach (var (index, value) in variables) peer.ConsoleValues[index] = value ? 1UL : 0;
        if (!peer.Synced || !_peers.Contains(peer) || peer.ConsoleId is not { } id) return;
        var sequence = peer.ConsoleSequence is { } previous ? AckWindow.SequenceNext(previous) : (byte)0;
        peer.ConsoleSequence = sequence;
        SendSequenced(peer, [NerdCodec.Write(NerdPayload.ConsoleUpdate(id, sequence, variables))]);
        if (peer.Objects.GetValueOrDefault(((byte)3, id)) is not { } obj) return;
        obj.Injected = AckWindow.SequenceNext(obj.Injected);
        foreach (var (index, value) in variables) obj.ServerVariables[index] = value;
        var payload = new BitBuffer();
        foreach (var (index, value) in variables) payload.Push(index, 4).Boolean(value);
        var message = NerdCodec.Write(new NerdMessage.Object(false, 3, new(HandleKind.Relay, id, peer.Index), 0,
            AckWindow.SequenceAdd(obj.Last, obj.Injected), payload));
        Forward(peer, [message], true);
    }

    private void RequestRace(RelayPeer peer)
    {
        if (_race is { } running)
        {
            // An event ends as soon as every participant finished or left, so a running one still has someone racing or loading.
            Note($"race request from peer {peer.Index}: ignored (peer {running.Owner}'s event is {Describe(running.Stage)})");
            return;
        }
        if (!IsEventOwner(peer))
        {
            Note(_eventOwner is { } owner
                ? $"race request from peer {peer.Index}: ignored (peer {owner} holds the event options)"
                : $"race request from peer {peer.Index}: ignored (it never requested the event options)");
            return;
        }
        foreach (var other in _peers.Where(p => p.Synced && p.ConsoleValues[4] != 0)) SetConsoleVariables(other, (4, false));
        var others = _peers.Any(p => p != peer && p.Synced);
        var wait = others ? _settings.RaceInviteMs : 0;
        _race = new(peer, _now + wait);
        Note($"race request from peer {peer.Index}: accepted, event opened ({_peers.Count(p => p != peer && p.Synced)} invited, {wait} ms to join)");
        if (others)
        {
            SetHostVariables((4, 1, 1), (6, 0, 1), (2, 1, 2), (3, unchecked(NowWire + (uint)wait), 32));
            Forward(peer, [NerdCodec.Write(new NerdMessage.Other(38, new()))], true);
        }
        else { SetHostVariables((4, 1, 1), (6, 0, 1)); SelectParticipants(); }
    }

    private void SelectParticipants()
    {
        if (_race is not { Stage: RaceStage.Inviting } race) return;
        race.Stage = RaceStage.Gathering;
        race.StageUntil = _now + _settings.RacePrepareMs;
        Note($"event: {race.Participants.Count} participant(s) selected, loading cars");
        BeginPhase(race.Participants);
        foreach (var peer in _peers.Where(p => race.Participants.Contains(p.Index)))
        { ResetConsoleObservation(peer, 5); ResetConsoleObservation(peer, 0); }
        SetHostVariables((5, 1, 1), (2, 0, 2));
        foreach (var index in race.Participants)
            if (_peers.FirstOrDefault(p => p.Index == index) is { } peer) SetConsoleVariables(peer, (2, true));
    }

    private static void ResetConsoleObservation(RelayPeer peer, byte variable)
    {
        peer.ConsoleValues[variable] = 0;
        // Pending object updates also establish causality. A delayed older report for a
        // previously unseen variable must not satisfy a new preparation/grid boundary.
        if (peer.ConsoleLatestSequence is { } latest) peer.ConsoleVariableSequences[variable] = latest;
    }

    private void Ready(RelayPeer peer, bool ready)
    {
        // The game waits for the answer to its Ready request (message 42: lobby key, then 0 = accepted).
        SendSequenced(peer, [NerdPayload.ReadyResponse(0)]);
        // Membership belongs to this invitation, independently of a previously cached flag.
        if (_race is { Stage: RaceStage.Inviting } race)
        {
            if (ready && !race.Participants.Contains(peer.Index)) race.Participants.Add(peer.Index);
            else if (!ready && peer.Index != race.Owner) race.Participants.Remove(peer.Index);
            Note($"ready request from peer {peer.Index}: ready={(ready ? 1 : 0)}, {race.Participants.Count} participant(s)");
        }
        else Note($"ready request from peer {peer.Index}: ready={(ready ? 1 : 0)} outside an invitation");
        if (peer.ConsoleValues[4] == (ready ? 1UL : 0) && peer.ConsoleSequence is not null) return;
        SetConsoleVariables(peer, (4, ready));
    }

    // The caller filters stale per-variable object sequences before reporting observations.
    private void ObserveConsole(RelayPeer peer, IReadOnlyList<(byte Variable, ulong Value)> values)
    {
        foreach (var (index, value) in values)
        {
            peer.ConsoleValues[index] = value;
            // RaceInProgress 0 after the event closed: this player left its results and is back in free roam
            if (index == 3 && value == 0 && _race is null) LeaveEventSide(peer);
            if (index != 3 || _race is not { Stage: RaceStage.Started } race || !race.Participants.Contains(peer.Index)) continue;
            if (value != 0) race.InProgress.Add(peer.Index);
            else if (race.InProgress.Remove(peer.Index))
            {
                // Initial/countdown zeroes are not departures. Only an observed active -> inactive transition is.
                RemoveRaceParticipant(peer);
            }
        }
    }

    private void RaceStep()
    {
        TickRace();
        if (_race is not { } race) return;
        bool All(int variable) => race.Participants.All(index => _peers.Any(p => p.Index == index && p.ConsoleValues[variable] != 0));
        if (race.Stage == RaceStage.Gathering && All(5))
        {
            // Gathering may have received readiness for an earlier route. The grid needs a new report.
            foreach (var peer in _peers.Where(p => race.Participants.Contains(p.Index))) ResetConsoleObservation(peer, 0);
            race.Stage = RaceStage.Grid;
            race.StageUntil = _now + _settings.RaceGridMs;
            Note($"event: every car loaded, starting grid sent to {race.Participants.Count}");
            foreach (var receiver in race.Participants)
            {
                var grid = race.Participants.Select(index => _peers.FirstOrDefault(p => p.Index == index))
                    .Where(p => p?.ConsoleId is not null)
                    .Select(p => new ObjectHandle(p!.Index == receiver ? HandleKind.Own : HandleKind.Relay, p.ConsoleId!.Value, p.Index == receiver ? (byte)0 : p.Index)).ToArray();
                if (_peers.FirstOrDefault(p => p.Index == receiver) is { } target) SendSequenced(target, [NerdPayload.StartingGrid(grid)]);
            }
        }
        else if (race.Stage == RaceStage.Grid && All(0))
        {
            race.Stage = RaceStage.Started;
            var start = unchecked(NowWire + 5000);
            foreach (var index in race.Participants)
                if (_peers.FirstOrDefault(p => p.Index == index) is { } peer) SendSequenced(peer, [NerdPayload.StartRace(start)]);
            SetHostVariables((6, 1, 1), (9, start, 32));
            Note($"event: race started with {race.Participants.Count}");
        }
    }

    private bool PassScore(RelayPeer peer, byte kind, BitBuffer body)
    {
        TickRace();
        if (kind is not (43 or 44 or 45) || _race is not { Stage: RaceStage.Started } race || !race.Participants.Contains(peer.Index)
            || NerdPayload.ScoreForOthers(body, peer.Index) is not { } payload) return false;
        var message = NerdCodec.Write(new NerdMessage.Other(kind, payload));
        foreach (var other in _peers.Where(p => p != peer && p.Synced && race.Participants.Contains(p.Index)))
            if (kind == 43) Send(other, [message], false); else SendSequenced(other, [message]);
        return true;
    }

    private void Finished(RelayPeer peer)
    {
        if (_race is not { Stage: RaceStage.Started } race || !race.Participants.Contains(peer.Index) || !race.Finished.Add(peer.Index)) return;
        race.FirstFinishAt ??= _now;
        ReevaluateRaceCompletion();
    }

    private void ReevaluateRaceCompletion()
    {
        if (_race is not { Stage: RaceStage.Started } race) return;
        if (race.Participants.Count == 0) EndRace("every participant left");
        else if (race.Participants.All(race.Finished.Contains)) EndRace("every participant finished");
        else if (race.FirstFinishAt is { } first && _now - first >= _settings.RaceFinishMs) EndRace("finish window over");
    }

    private static string Describe(RaceStage stage) => stage switch
    {
        RaceStage.Inviting => "inviting", RaceStage.Gathering => "loading cars", RaceStage.Grid => "on the grid", _ => "racing",
    };

    private void EndRace(string why)
    {
        if (_race is not { } race) return;
        _race = null; ClearGameOptions();
        // The owner's game still holds the event options after its event and asks for them again only when it lost them, so it
        // keeps them for its next race request; another player's ownership request takes them over between events.
        if (_eventOwner == race.Owner && !_peers.Contains(race.OwnerPeer)) _eventOwner = null;
        Note($"event closed: {why}");
        EventClosed(race);
        SetHostVariables((2, 0, 2), (4, 0, 1), (5, 0, 1), (6, 0, 1));
        foreach (var index in race.Participants)
            if (_peers.FirstOrDefault(p => p.Index == index) is { } peer) SetConsoleVariables(peer, (2, false), (4, false));
    }

    private void RemoveRaceParticipant(RelayPeer peer)
    {
        if (_race is not { } race || !race.Participants.Remove(peer.Index)) return;
        race.Finished.Remove(peer.Index); race.InProgress.Remove(peer.Index);
        SetConsoleVariables(peer, (2, false), (4, false));
        LeaveEventSide(peer);
        if (race.Participants.Count == 0) EndRace("every participant left");
        else if (ReferenceEquals(peer, race.OwnerPeer) && race.Stage != RaceStage.Started) EndRace("its owner left before the start");
        else ReevaluateRaceCompletion();
    }

    private void RacePeerLeft(RelayPeer peer)
    {
        if (_race is null)
        {
            if (_eventOwner == peer.Index) _eventOwner = null;
            return;
        }
        // The active event retains exclusive ownership until completion, even if its owner disconnects.
        RemoveRaceParticipant(peer);
        RaceStep();
    }

    private void TickRace()
    {
        if (_race is not { } race) return;
        if (race.Stage == RaceStage.Inviting && _now >= race.InviteUntil) SelectParticipants();
        else if (race.Stage is RaceStage.Gathering or RaceStage.Grid && _now >= race.StageUntil) StageTimedOut(race);
        else ReevaluateRaceCompletion();
    }

    /// <summary>
    /// Loading or the grid ran out of time. Those who reported keep the event: only the stuck players leave it. A game reports its
    /// cars loaded only once it has loaded every other participant's car, so when nobody reported, one car may hold them all: the
    /// newest joiner other than the owner leaves first, and the stage gets another step. With nobody left but the owner, it ends.
    /// </summary>
    private void StageTimedOut(Race race)
    {
        var variable = race.Stage == RaceStage.Gathering ? 5 : 0;
        bool Reported(byte index) => _peers.FirstOrDefault(p => p.Index == index) is { } p && p.ConsoleValues[variable] != 0;
        var stuck = race.Participants.Where(index => !Reported(index)).ToList();
        if (stuck.Count == 0) return;
        var stage = Describe(race.Stage);
        List<byte> drop;
        if (stuck.Count < race.Participants.Count) drop = stuck;
        else if (stuck.FindLast(index => index != race.Owner) is var suspect and not 0) drop = [suspect];
        else { EndRace($"nobody finished {stage} in time"); return; }
        race.StageUntil = _now + Math.Min(_settings.RacePrepareMs, StuckStepMs);
        Note($"event: {stage} timed out, peer(s) {string.Join(",", drop)} leave the event");
        foreach (var index in drop)
            if (_peers.FirstOrDefault(p => p.Index == index) is { } peer) RemoveRaceParticipant(peer);
            else race.Participants.Remove(index);
        if (_race == race) RaceStep();
    }

    private const long StuckStepMs = 10000;
}

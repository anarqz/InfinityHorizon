// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.WorldServer;

/// <summary>A car meet's request for the shared game-mode session its invitation leads to: the starter first, then the joiners.</summary>
internal sealed record CarMeetSessionRequest(uint Id, byte Kind, IReadOnlyList<ulong> Xuids);

/// <summary>
/// Car meet game-mode invitations (CarMeetSynced on router 11). A player starts Online Free Roam from the meet (MsgCarMeetPlayerInvite-
/// Request 26); the others get a 20 s countdown and join or cancel (27). When it ends, the platform creates one shared session for the
/// starter and everybody who joined. The joiners are told to wait (MsgCarMeetNotifyPending 29, wait) and then where to go
/// (MsgCarMeetSessionReady 32, plus CarMeetSynced vars 16/17). Any failure, or the starter leaving, cancels it for everybody (invite
/// type 0). Updates go out reliably, so the state is never re-broadcast on a timer.
/// </summary>
internal sealed partial class RelayEngine
{
    internal const byte InviteFreeRoam = 1;
    internal const long CarMeetJoinMs = 20000, CarMeetCreateMs = 15000, CarMeetReadyMs = 30000;
    private enum InviteStage { Idle, Joining, Creating, Ready }
    private InviteStage _invite;
    private byte _inviteKind;
    private RelayPeer? _inviteStarter;
    private readonly List<RelayPeer> _inviteJoined = [];
    private long _inviteUntil;
    private uint _inviteMode32, _inviteRequest;
    private string? _sessionName, _sessionTemplate;
    private byte _carMeetSequence;

    /// <summary>Asks the platform for the shared session; the answer comes back through <see cref="GameSessionCreated"/>.</summary>
    internal Action<CarMeetSessionRequest>? CreateGameSession { get; set; }

    /// <summary>CarMeetSynced as it is now (late joiners get exactly this in their snapshot).</summary>
    private BitBuffer CarMeetState()
    {
        if (_invite == InviteStage.Idle) return NerdPayload.CarMeet();
        var output = new BitBuffer().Push(0, 5).Push(_inviteKind, 2);
        output.Append(NerdPayload.CarMeet(_sessionName, _sessionTemplate, CarMeetJoinMs));
        return output.Push(3, 5).Push(_inviteMode32, 32);
    }

    private void CarMeetUpdate(BitBuffer variables)
    {
        _carMeetSequence = AckWindow.SequenceNext(_carMeetSequence);
        var update = NerdCodec.Write(new NerdMessage.Object(false, 11, new(HandleKind.Own, 0), 0, _carMeetSequence, variables));
        foreach (var peer in _peers.Where(p => p.Synced)) SendSequenced(peer, [update]);
    }

    private void InviteRequest(RelayPeer peer, BitBuffer body)
    {
        if (_settings.CarMeetLobby is null) return;
        var reader = new BitReader(body.Bytes, body.Length);
        var kind = (byte)reader.Read(2);
        if (kind != InviteFreeRoam)
        {
            // Road Trip and Showdown (wager race) invitations come later: their sessions do not exist on the platform yet
            Note($"car meet: invitation type {kind} from peer {peer.Index} ignored (not served yet)");
            return;
        }
        if (_invite != InviteStage.Idle) { Note($"car meet: invitation from peer {peer.Index} ignored (one is {_invite})"); return; }
        _invite = InviteStage.Joining; _inviteKind = kind; _inviteStarter = peer;
        _inviteJoined.Clear(); _inviteJoined.Add(peer);
        _inviteUntil = _now + CarMeetJoinMs;
        _inviteMode32 = unchecked(NowWire + (uint)CarMeetJoinMs);
        _sessionName = _sessionTemplate = null;
        CarMeetUpdate(new BitBuffer().Push(0, 5).Push(kind, 2).Push(1, 5).Push((ulong)CarMeetJoinMs, 32).Push(3, 5).Push(_inviteMode32, 32));
        Note($"car meet: peer {peer.Index} invites to online free roam, {CarMeetJoinMs} ms to join");
    }

    private void JoinOrCancel(RelayPeer peer, BitBuffer body)
    {
        if (_invite != InviteStage.Joining || peer == _inviteStarter) return;
        var reader = new BitReader(body.Bytes, body.Length);
        var bit = reader.Read(1) != 0;
        var type = reader.Read(2);
        if (type == 1) return; // a wager-race answer
        // For game-mode invitations the bit reads the other way round (FH2MP, observed in game): accepting arrives as 0, and the
        // game's own automatic 0-then-1 at the start therefore ends as "not joined" for a player who pressed nothing.
        var join = !bit;
        if (join && !_inviteJoined.Contains(peer)) _inviteJoined.Add(peer);
        else if (!join) _inviteJoined.Remove(peer);
        Note($"car meet: peer {peer.Index} {(join ? "joined" : "left")} the invitation ({_inviteJoined.Count} in)");
    }

    private void TickCarMeet()
    {
        switch (_invite)
        {
            case InviteStage.Joining when _inviteStarter is null || !_peers.Contains(_inviteStarter):
                CancelInvite("its starter left");
                break;
            case InviteStage.Joining when _now >= _inviteUntil:
                _inviteJoined.RemoveAll(p => !_peers.Contains(p));
                if (CreateGameSession is not { } create) { CancelInvite("this host cannot create sessions"); break; }
                _invite = InviteStage.Creating;
                _inviteUntil = _now + CarMeetCreateMs;
                _inviteRequest = unchecked(_inviteRequest + 1);
                // nobody creates the session on the game side: everybody waits for the one the platform makes
                foreach (var peer in _inviteJoined) SendSequenced(peer, [NerdPayload.CarMeetNotifyPending(1)]);
                Note($"car meet: countdown over, {_inviteJoined.Count} in, asking for the session");
                create(new(_inviteRequest, _inviteKind, _inviteJoined.Select(p => p.Xuid).ToArray()));
                break;
            case InviteStage.Creating when _now >= _inviteUntil:
                CancelInvite("the session did not arrive in time");
                break;
            case InviteStage.Ready when _now >= _inviteUntil:
                _invite = InviteStage.Idle; _inviteStarter = null; _inviteJoined.Clear();
                _sessionName = _sessionTemplate = null;
                CarMeetUpdate(new BitBuffer().Push(0, 5).Push(0, 2));
                break;
        }
    }

    /// <summary>The platform's answer for request <paramref name="id"/>: the shared session, or null when it was refused.</summary>
    internal void GameSessionCreated(uint id, string? name, string? template, long nowMilliseconds)
    {
        _now = Math.Max(_now, nowMilliseconds);
        if (_invite != InviteStage.Creating || id != _inviteRequest) return;
        if (name is null || template is null || !Printable(name) || !Printable(template)) { CancelInvite("the platform refused the session"); return; }
        _invite = InviteStage.Ready;
        _inviteUntil = _now + CarMeetReadyMs;
        _sessionName = name; _sessionTemplate = template;
        CarMeetUpdate(NerdPayload.CarMeet(name, template, CarMeetJoinMs));
        var ready = NerdPayload.CarMeetSessionReady(name, template);
        foreach (var peer in _inviteJoined.Where(_peers.Contains)) SendSequenced(peer, [ready]);
        Note($"car meet: session ready, sent to {_inviteJoined.Count(_peers.Contains)}");
    }

    private static bool Printable(string text) => text.Length is > 0 and <= NerdPayload.CarMeetNameMax && text.All(c => c is >= ' ' and <= '~');

    private void CancelInvite(string why)
    {
        _invite = InviteStage.Idle; _inviteStarter = null; _inviteJoined.Clear();
        _sessionName = _sessionTemplate = null;
        CarMeetUpdate(new BitBuffer().Push(0, 5).Push(0, 2));
        Note($"car meet: invitation cancelled ({why})");
    }

    private void CarMeetPeerLeft(RelayPeer peer) => _inviteJoined.Remove(peer);
}

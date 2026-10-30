// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.WorldServer;

internal sealed partial class RelayEngine
{
    private readonly RelayGameOptionsState _gameOptions = new();

    private List<BitBuffer> Ownership(RelayPeer peer, BitBuffer body)
    {
        TickGameOptions();
        if (peer.Doomed) return [];
        if (NerdPayload.Ownership(body) is not { Router: 6, Type: 1, Handle.Kind: HandleKind.Transfer } request) return [];
        // A repeated ownership grant must not jump over an unresolved source fragment.
        if (IsEventOwner(peer) && _gameOptions.PendingCount != 0) { Note($"options ownership request from peer {peer.Index}: held (fragments pending)"); return []; }
        var previous = peer.LobbySequences.GetValueOrDefault(request.Handle.Id, (Sequence: (byte)254, At: _now - 2000));
        if (_now - previous.At < 2000) return [];
        var sequence = AckWindow.SequenceNext(previous.Sequence);
        peer.LobbySequences[request.Handle.Id] = (sequence, _now);
        if (_eventOwner is { } current && !IsEventOwner(peer))
        {
            // During an event its owner keeps the options. Between events they go to whoever opens the event menu next: the
            // previous owner keeps them after its event (its game asks again only when it lost them), so nobody else waits on it.
            if (_race is not null)
            {
                Note($"options ownership request from peer {peer.Index}: refused (peer {current} runs an event)");
                return [NerdPayload.TransferOwnership(6, request.Handle, sequence)];
            }
            Note($"options ownership moves from peer {current} to peer {peer.Index} (no event running)");
        }
        else Note($"options ownership granted to peer {peer.Index}");
        _eventOwner = peer.Index;
        _gameOptions.BeginSourceEpoch(peer);
        if (_gameOptions.ObjectId != request.Handle.Id || _gameOptions.Records.Count == 0)
            return [NerdPayload.GrantOwnership(6, request.Handle, sequence, new())];

        // Each record keeps its original, valid 14-bit length. A grant carries the first
        // fragment, followed by ordinary reliable updates in the recipient's sequence domain.
        var replay = new List<BitBuffer>(_gameOptions.Records.Count)
        {
            NerdPayload.GrantOwnership(6, request.Handle, sequence, _gameOptions.Records[0]),
        };
        foreach (var payload in _gameOptions.Records.Skip(1))
            replay.Add(GameOptionsUpdate(peer, request.Handle.Id, payload));
        return replay;
    }

    private void OnGameOptions(RelayPeer peer, NerdMessage.Object obj)
    {
        TickGameOptions();
        if (peer.Doomed || !IsEventOwner(peer) || obj.Create) return;
        var result = _gameOptions.Apply(peer, obj.Handle.Id, obj.Sequence, obj.Payload, _now, out var ready);
        if (result == GameOptionsUpdateResult.Overflow)
        {
            Rejected(peer, "game_options_overflow", obj.Id, obj.Router);
            // Dropping only the overflowing fragment would leave a live, incomplete object
            // that no late join or new owner could recover. End this writer's session instead.
            peer.Doomed = true;
            return;
        }
        if (result != GameOptionsUpdateResult.Stored) return;
        foreach (var other in _peers.Where(p => p != peer && p.Synced))
            SendSequenced(other, ready.Select(payload => GameOptionsUpdate(other, obj.Handle.Id, payload)).ToArray());
    }

    private BitBuffer GameOptionsUpdate(RelayPeer peer, uint id, BitBuffer payload)
    {
        var previous = peer.LobbySequences.GetValueOrDefault(id, (Sequence: (byte)254, At: _now - 2000));
        var next = AckWindow.SequenceNext(previous.Sequence);
        peer.LobbySequences[id] = (next, previous.At);
        return NerdCodec.Write(new NerdMessage.Object(false, 6, new(HandleKind.Transfer, id), 0, next, payload));
    }

    private void AppendGameOptionsSnapshot(RelayPeer peer, List<BitBuffer> snapshot)
    {
        if (_gameOptions.ObjectId is not { } id) return;
        foreach (var payload in _gameOptions.Records) snapshot.Add(GameOptionsUpdate(peer, id, payload));
    }

    private void ClearGameOptions() => _gameOptions.Clear();

    private void TickGameOptions()
    {
        if (_gameOptions.ExpiredSource(_now, _settings.GameOptionsGapMs) is { } owner) owner.Doomed = true;
    }
}

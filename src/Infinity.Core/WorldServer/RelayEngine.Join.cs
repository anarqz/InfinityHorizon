// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;

namespace Infinity.Core.WorldServer;

/// <summary>
/// Quiet joins. The players already in a world reach a joiner in its handshake snapshot, so they are there as it enters; their cars
/// (router 2), the event's game options and the live car traffic wait until the joiner's FinaliseSessionEntry. Until then each joiner
/// is held: nothing of that kind goes out to it. At the finalise it gets the then-current snapshot of what was held, so no update is
/// lost or reordered; a car deleted in the window was never sent. A joiner that never finalises is released after
/// <see cref="QuietJoinBackstopMs"/>, so it is never left without cars.
/// </summary>
/// <remarks>
/// Sent at the handshake and kept live through the window (the joiner holds their creates, and their sequences continue): the world's
/// own objects (SessionHost, weather, car meet, Road Trip, the bucket-list slot array) and the players' routers 1, 3, 9 and 12. Held:
/// router-2 objects and their deletes, car predictions and car events (contacts, glass, repairs: as if the joiner had come later), and
/// the game options' updates (<see cref="OnGameOptions"/> skips a held joiner, so its options sequence never moves while held).
/// The cars' last predictions follow only once every reliable message of the release has been acknowledged, so a parked car's
/// prediction never reaches the game before its object.
/// </remarks>
internal sealed partial class RelayEngine
{
    /// <summary>
    /// A joiner that has not sent FinaliseSessionEntry this long after its sync is released anyway. A real game finalises a few hundred
    /// milliseconds after its own creates; this is far past that, and past <see cref="RelaySettings.SyncCompleteMs"/>'s default.
    /// </summary>
    internal const long QuietJoinBackstopMs = 15000;

    /// <summary>
    /// The cars' last predictions wait at most this long for the release's acknowledgements; then they go anyway, logged once. A reliable
    /// packet is resent up to 20 times (about 10 s at the default 500 ms), so a joiner still unacknowledged here is about to be dropped.
    /// </summary>
    internal const long CarsPendingMs = 10000;

    private enum JoinRelease { Finalise, Backstop, Ready }

    /// <summary>The players whose join is held, with when they synced.</summary>
    private readonly Dictionary<RelayPeer, long> _holding = [];

    /// <summary>Released joiners waiting for the acknowledgement of their release's reliable messages (by reference) before the cars.</summary>
    private readonly Dictionary<RelayPeer, (HashSet<BitBuffer> Release, long Since)> _carsPending = [];

    /// <summary>Cars (owner index, prediction key) whose last prediction was skipped for a missing object, logged once each (bounded).</summary>
    private readonly HashSet<(byte Owner, uint Id)> _skippedCarsNoted = [];

    /// <summary>Whether this message waits for its recipient's FinaliseSessionEntry (see the class remarks).</summary>
    private static bool HeldForJoiner(BitBuffer message)
    {
        var reader = new BitReader(message.Bytes, message.Length);
        if (!NerdCodec.TryRead(ref reader, out var read, out _)) return false;
        return read is NerdMessage.Object { Router: 2, Handle.Kind: HandleKind.Relay } or NerdMessage.Delete { Router: 2, Handle.Kind: HandleKind.Relay }
            or NerdMessage.Car or NerdMessage.CarEvent;
    }

    /// <summary>What may go to <paramref name="peer"/> now: everything once it is released, else what is not held.</summary>
    private IReadOnlyList<BitBuffer> Unheld(RelayPeer peer, IReadOnlyList<BitBuffer> messages)
    {
        if (!_holding.ContainsKey(peer)) return messages;
        List<BitBuffer>? kept = null;
        for (var i = 0; i < messages.Count; i++)
        {
            if (!HeldForJoiner(messages[i])) { kept?.Add(messages[i]); continue; }
            kept ??= [.. messages.Take(i)];
        }
        return kept ?? messages;
    }

    private void HoldJoiner(RelayPeer peer) => _holding[peer] = _now;

    /// <summary>The joiner's session entry is final (or its backstop ran out, or it readied for an event): what was held goes out, as it is now.</summary>
    private void ReleaseJoiner(RelayPeer peer, JoinRelease why)
    {
        if (!_holding.Remove(peer, out var since) || !_peers.Contains(peer)) return;
        var held = new List<BitBuffer>();
        foreach (var owner in _peers.Where(p => p != peer && p.Synced))
            foreach (var (key, obj) in owner.Objects.Where(p => p.Key.Router == 2))
                held.AddRange(obj.Snapshot(key.Router, new(HandleKind.Relay, key.Id, owner.Index)));
        // the journal, after whatever this joiner was really sent in that sequence domain (an ownership answer included); its own
        // grant replay already carried the journal to an owner
        if (_gameOptions.ObjectId is not null && !IsEventOwner(peer)) AppendGameOptionsSnapshot(peer, held);
        foreach (var message in held)
            if (!QueueReliable(peer, message)) break;
        Pace(peer);
        if (peer.Doomed) return;
        _carsPending[peer] = (new HashSet<BitBuffer>(held, ReferenceEqualityComparer.Instance), _now);
        Note(why switch
        {
            JoinRelease.Backstop => string.Create(CultureInfo.InvariantCulture, $"peer {peer.Index} sent no FinaliseSessionEntry within {QuietJoinBackstopMs} ms: its held join is released ({held.Count} message(s))"),
            JoinRelease.Ready => string.Create(CultureInfo.InvariantCulture, $"peer {peer.Index} readied for an event before its FinaliseSessionEntry: its held join is released ({held.Count} message(s))"),
            _ => string.Create(CultureInfo.InvariantCulture, $"peer {peer.Index} finalised its session entry {_now - since} ms after its sync: {held.Count} held message(s) sent"),
        });
        CheckCarsPending();
    }

    /// <summary>Released joiners whose release was acknowledged (or waited <see cref="CarsPendingMs"/>) get the cars' last predictions.</summary>
    private void CheckCarsPending()
    {
        if (_carsPending.Count == 0) return;
        foreach (var (peer, (release, since)) in _carsPending.ToArray())
        {
            if (!_peers.Contains(peer) || peer.Doomed) { _carsPending.Remove(peer); continue; }
            var unacknowledged = release.Count != 0 && (peer.Queue.Any(release.Contains) || peer.InFlight.Any(p => p.Messages.Any(release.Contains)));
            if (unacknowledged && _now - since < CarsPendingMs) continue;
            _carsPending.Remove(peer);
            if (unacknowledged)
                Note(string.Create(CultureInfo.InvariantCulture, $"peer {peer.Index}: its release is still unacknowledged after {CarsPendingMs} ms, the cars' last predictions go anyway"));
            SendLastCars(peer);
        }
    }

    /// <summary>
    /// Each other car's last prediction, one tick newer (the game ignores one that is not newer than the last it applied), through
    /// <see cref="CarFor"/> so it carries the right universe. The newer stamp is kept as the car's last, so a later refresh is newer
    /// still for everybody. A car whose object is gone is skipped.
    /// </summary>
    private void SendLastCars(RelayPeer peer)
    {
        foreach (var sender in _peers.Where(p => p != peer && p.Synced && p.LastCar is not null).ToArray())
        {
            var car = sender.LastCar!;
            if (!sender.Objects.ContainsKey((2, car.Handle.Id)))
            {
                // logged once per car: a parked car that never shows would point at the key/object-id assumption
                if (_skippedCarsNoted.Count < 1024 && _skippedCarsNoted.Add((sender.Index, car.Handle.Id)))
                    Note(string.Create(CultureInfo.InvariantCulture, $"peer {sender.Index} car {car.Handle.Id}: no router-2 object with that id, its last prediction is not replayed to peer {peer.Index}"));
                continue;
            }
            var reader = new BitReader(car.Body.Bytes, car.Body.Length);
            reader.Skip(CarStampOffset);
            var stamp = unchecked((uint)reader.Read(32) + 1);
            sender.LastCar = car = car with { Body = WithBits(car.Body, CarStampOffset, 32, stamp) };
            Send(peer, [CarFor(sender, peer, car)], false);
        }
    }

    private void TickQuietJoins()
    {
        if (_holding.Count != 0)
            foreach (var (peer, since) in _holding.ToArray())
                if (_now - since >= QuietJoinBackstopMs) ReleaseJoiner(peer, JoinRelease.Backstop);
        CheckCarsPending();
    }
}

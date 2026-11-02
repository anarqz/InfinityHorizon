// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.WorldServer;

/// <summary>
/// Bucket-list co-op in free roam (router 12, 4 id bits, 2 type bits). Each player's own BucketListPlayerStateSynced (type 0, id 0) is
/// relayed, sanitized, never owned by anyone else. The server owns BucketListSlotArraySynced (type 1, id 0): 12 entries of SlotId,
/// ChallengeId, OwnerSyncId, which give the other players' games their "Join" prompt. A player asks for a partner (RequestSynchronize
/// 63); the first player waiting on the same slot and challenge is its partner (65 to both). The start time (61) goes to both only once
/// both PlayerStates report State 2, once per pair: sent with 65 it is lost to the game's own RequestChallenge. ChallengeEnded (62) or a
/// leave ends the pair; a cancel (64) ends only a wait, since the game sends one right after it is paired. Waiting and paired players stay
/// in universe 0, also while an event phases the others.
/// </summary>
/// <remarks>
/// Server output produced while a packet is handled (65, 61, 66-68 and the slot array's one delta) is staged and sent after that packet's
/// relayed objects (<see cref="FlushBucketList"/>), so a partner's PlayerState create always precedes the 65 that names it.
/// </remarks>
internal sealed partial class RelayEngine
{
    internal const byte BucketListRouter = 12, BucketListPlayerType = 0, BucketListSlotsType = 1;
    internal const int BucketListEntries = 12;
    internal const long BucketListWaitMs = 120000;
    internal const uint BucketListStartDelayMs = 3000;
    /// <summary>A player's RequestSynchronize (63) is taken at most this often; extra ones are ignored (noted once).</summary>
    internal const long BucketListRequestMinMs = 1000;
    /// <summary>A pair not started this long after its pairing is logged once (V11 diagnostic: who has not reported State 2).</summary>
    internal const long BucketListStartStallMs = 15000;
    /// <summary>BucketListPlayerStateSynced.State once the player's game is ready to start the challenge.</summary>
    internal const int BucketListReadyState = 2;
    internal const int BucketListNameMax = 36;
    /// <summary>The 5-bit start positions in 65: the first waiter's, then its partner's. Unverified; the two-client capture (V11) decides.</summary>
    internal const byte BucketListFirstStart = 0, BucketListSecondStart = 1;
    /// <summary>Whether the s32 in 66-68 is the slot (true) or the challenge (false). Unverified; the two-client capture (V11) decides.</summary>
    internal const bool BucketListStatusCarriesSlot = false;
    /// <summary>
    /// What 65's string carries: the partner's name as this relay knows it (its player object's NetworkName). Unverified; the two-client
    /// capture (V11) decides.
    /// </summary>
    internal const bool BucketListPartnerNameIsGamertag = true;
    /// <summary>
    /// Whether the first waiter's entry stays while its pair runs (false: freed at the pairing, so a full pair shows no "Join" to the
    /// others). Unverified; the two-client capture (V11) decides.
    /// </summary>
    internal const bool BucketListEntryKeptWhilePaired = false;

    private sealed class BucketPlayer
    {
        internal bool HasState { get; set; }
        /// <summary>The newest PlayerState sequence seen (create or update).</summary>
        internal byte? Latest { get; set; }
        /// <summary>Per variable, the sequence its value came from: an update arriving late never undoes a newer value.</summary>
        internal Dictionary<byte, byte> Sequences { get; } = [];
        internal int State { get; set; }
        internal int Slot { get; set; }
        internal int Challenge { get; set; }
        internal bool RequestingPartner { get; set; }
        /// <summary>The player object's last NetworkName, read from each relayed router-1 payload on its own.</summary>
        internal string? Name { get; set; }
        /// <summary>A RequestingPartner without 63, logged once for this slot and challenge.</summary>
        internal (int Slot, int Challenge)? Unrequested { get; set; }
        internal (int Slot, int Challenge, long At, long Order)? Waiting { get; set; }
        internal BucketPair? Pair { get; set; }
        /// <summary>When the last 63 was applied; cleared when a 64 or 62 ends what it asked for.</summary>
        internal long? LastRequestAt { get; set; }
        /// <summary>The newest 63 that came inside the rate window: applied when the window passes (the game's latest request wins).</summary>
        internal (int Slot, int Challenge)? PendingRequest { get; set; }
        internal bool RateNoted { get; set; }
        internal bool RepeatNoted { get; set; }
        internal bool EarlyNoted { get; set; }
        internal bool RefusalNoted { get; set; }
        internal bool CapNoted { get; set; }
    }

    private sealed class BucketPair(RelayPeer first, RelayPeer second, int slot, int challenge, long pairedAt)
    {
        internal RelayPeer First { get; } = first;
        internal RelayPeer Second { get; } = second;
        internal int Slot { get; } = slot;
        internal int Challenge { get; } = challenge;
        internal long PairedAt { get; } = pairedAt;
        /// <summary>Each partner's newest PlayerState sequence at the pairing: only a State 2 from a later update counts.</summary>
        internal Dictionary<RelayPeer, byte?> Floors { get; } = [];
        internal HashSet<RelayPeer> Ready { get; } = [];
        internal bool Started { get; set; }
        internal bool StallNoted { get; set; }
    }

    private readonly Dictionary<RelayPeer, BucketPlayer> _bucketPlayers = [];
    private readonly (int Slot, int Challenge, int Owner)[] _bucketSlots = new (int, int, int)[BucketListEntries];
    /// <summary>The slot array as the players have it (the last delta sent): joiners get this, at its sequence.</summary>
    private readonly (int Slot, int Challenge, int Owner)[] _bucketSlotsSent = new (int, int, int)[BucketListEntries];
    private readonly List<(RelayPeer To, BitBuffer Message)> _bucketOutbox = [];
    private byte _bucketSlotsSequence;
    private long _bucketOrder;

    /// <summary>Bucket-list co-op is free roam's: no car meet or Road Trip world serves it. Not behind <see cref="RelaySettings.Races"/>.</summary>
    private bool BucketListWorld => _settings.CarMeetLobby is null && !IsRoadTrip;

    /// <summary>A waiting or paired player keeps universe 0 (no private universe for it while an event phases the others).</summary>
    private bool BucketListBusy(RelayPeer peer) => _bucketPlayers.TryGetValue(peer, out var player) && (player.Waiting is not null || player.Pair is not null);

    private BucketPlayer BucketPlayerOf(RelayPeer peer)
    {
        if (!_bucketPlayers.TryGetValue(peer, out var player)) _bucketPlayers[peer] = player = new();
        return player;
    }

    private static int BucketListStatusValue(int slot, int challenge) => BucketListStatusCarriesSlot ? slot : challenge;

    private static IEnumerable<(byte Variable, int Value)> BucketListEntryVariables((int Slot, int Challenge, int Owner)[] slots, int i) =>
        [((byte)(3 * i), slots[i].Slot), ((byte)(3 * i + 1), slots[i].Challenge), ((byte)(3 * i + 2), slots[i].Owner)];

    private void AppendBucketListSnapshot(List<BitBuffer> snapshot)
    {
        if (!BucketListWorld) return;
        // each used entry's three variables, ascending; unused entries keep the constructor's 0, so an empty table is an empty create
        var payload = NerdPayload.BucketListSlots(Enumerable.Range(0, BucketListEntries).Where(i => _bucketSlotsSent[i] != default)
            .SelectMany(i => BucketListEntryVariables(_bucketSlotsSent, i)));
        snapshot.Add(NerdCodec.Write(new NerdMessage.Object(true, BucketListRouter, new(HandleKind.Own, 0), BucketListSlotsType,
            _bucketSlotsSequence, payload)));
    }

    private void BucketListSend(RelayPeer to, BitBuffer message) => _bucketOutbox.Add((to, message));

    /// <summary>Players whose wait or pair began or ended: their universe is settled at the flush, from their final state then.</summary>
    private readonly HashSet<RelayPeer> _bucketUniverseDue = [];

    /// <summary>
    /// A waiting or paired player is at universe 0; one that no longer is gets a private universe while phasing, as everybody else. Run at
    /// the flush, after the packet's relayed objects: a player whose router-1 create came in this packet (UniverseDue) only has its value
    /// set, and PlayerObjectCreated sends it after that create.
    /// </summary>
    private void BucketListUniverse(RelayPeer peer)
    {
        if (!_peers.Contains(peer)) return;
        if (BucketListBusy(peer))
        {
            if (peer.Universe == 0) return;
            if (peer.UniverseDue) peer.Universe = 0; else SetUniverse(peer, 0);
        }
        else if (_phasing && peer.Universe == 0)
        {
            if (!peer.UniverseDue) AssignUniverse(peer);
            else if (FreeUniverse() is { } universe) peer.Universe = universe;
        }
    }

    private void BucketListStatus(byte kind, int slot, int challenge, params RelayPeer[] except)
    {
        var message = NerdPayload.BucketListStatus(kind, BucketListStatusValue(slot, challenge));
        foreach (var peer in _peers.Where(p => p.Synced && !except.Contains(p))) BucketListSend(peer, message);
    }

    /// <summary>The staged output: the slot array's one delta (only the variables that differ from what was sent), then the messages.</summary>
    private void FlushBucketList()
    {
        if (_bucketUniverseDue.Count != 0)
        {
            var due = _bucketUniverseDue.ToArray(); _bucketUniverseDue.Clear();
            foreach (var peer in due) BucketListUniverse(peer);
        }
        var changed = Enumerable.Range(0, BucketListEntries).SelectMany(i => BucketListEntryVariables(_bucketSlots, i)
            .Zip(BucketListEntryVariables(_bucketSlotsSent, i)).Where(p => p.First.Value != p.Second.Value).Select(p => p.First)).ToList();
        if (changed.Count != 0)
        {
            Array.Copy(_bucketSlots, _bucketSlotsSent, BucketListEntries);
            _bucketSlotsSequence = AckWindow.SequenceNext(_bucketSlotsSequence);
            var update = NerdCodec.Write(new NerdMessage.Object(false, BucketListRouter, new(HandleKind.Own, 0), 0, _bucketSlotsSequence,
                NerdPayload.BucketListSlots(changed)));
            foreach (var peer in _peers.Where(p => p.Synced)) SendSequenced(peer, [update]);
        }
        if (_bucketOutbox.Count == 0) return;
        var outbox = _bucketOutbox.ToArray(); _bucketOutbox.Clear();
        foreach (var (to, message) in outbox) if (to.Synced && _peers.Contains(to)) SendSequenced(to, [message]);
    }

    private void FreeBucketEntries(params RelayPeer[] peers)
    {
        var owners = peers.Select(p => (int)p.Index).ToHashSet();
        for (var i = 0; i < BucketListEntries; i++) if (_bucketSlots[i].Owner != 0 && owners.Contains(_bucketSlots[i].Owner)) _bucketSlots[i] = default;
    }

    private void ClaimBucketEntry(RelayPeer peer, int slot, int challenge)
    {
        if (slot == 0) { Note($"bucket list: peer {peer.Index} waits on slot 0, challenge {challenge}: no entry"); return; }
        var free = Array.FindIndex(_bucketSlots, e => e.Owner == 0);
        if (free < 0) { Note($"bucket list: all {BucketListEntries} entries taken, peer {peer.Index} waits without one (it can still be paired)"); return; }
        _bucketSlots[free] = (slot, challenge, peer.Index);
    }

    /// <summary>A router-12 object refused (any other shape, or any in a world without co-op): noted once per peer outside free roam.</summary>
    private void BucketListRefused(RelayPeer peer, NerdMessage.Object obj)
    {
        var player = BucketPlayerOf(peer);
        // outside free roam, or after its PlayerState create hit the object cap (its updates refused from then on): once per peer
        if ((!BucketListWorld || player.CapNoted) && player.RefusalNoted) return;
        player.RefusalNoted = true;
        Note($"bucket list: peer {peer.Index} router-12 object refused (create {obj.Create}, type {obj.Type}, {obj.Handle.Kind} id {obj.Handle.Id}, {obj.Payload.Length} bits)");
    }

    /// <summary>A PlayerState create refused at the 256-object cap: noted once per peer.</summary>
    private void BucketListCapped(RelayPeer peer)
    {
        var player = BucketPlayerOf(peer);
        if (player.CapNoted) return;
        player.CapNoted = true; player.RefusalNoted = false; // the refusals that follow get one note of their own
        Note($"bucket list: peer {peer.Index} holds 256 objects, its PlayerState create is refused (noted once)");
    }

    /// <summary>A relayed router-1 payload: the NetworkName it carries, if any, read from that payload's own start.</summary>
    private void BucketListPlayerObject(RelayPeer peer, BitBuffer payload)
    {
        if (BucketListWorld && NerdPayload.PlayerName(payload) is { } name) BucketPlayerOf(peer).Name = name;
    }

    /// <summary>
    /// A player's own BucketListPlayerStateSynced, sanitized and accepted (relayed objects accept an update ahead of a gap, so updates come
    /// in arrival order): each variable takes a value only from a sequence newer than its last one. A State 2 counts as ready for the
    /// pair when its update is newer than the pairing, even if it arrives late.
    /// </summary>
    private void BucketListPlayerUpdate(RelayPeer peer, bool create, byte sequence, BitBuffer payload)
    {
        var player = BucketPlayerOf(peer);
        if (create)
        {
            player.State = 0; player.Slot = 0; player.Challenge = 0; player.RequestingPartner = false;
            player.Sequences.Clear(); player.Latest = sequence;
            // a recreated PlayerState numbers afresh: the pair's floor moves to this create (which counts itself if it says State 2)
            if (player.Pair is { Started: false } running) running.Floors[peer] = sequence;
        }
        else if (player.Latest is not { } latest || AckWindow.SequenceNewer(sequence, latest)) player.Latest = sequence;
        player.HasState = true;
        var reportsReady = false; // this update itself carries State 2 (a remembered State 2 never counts)
        foreach (var (variable, value) in NerdPayload.BucketListPlayerOf(payload) ?? [])
        {
            if (variable == NerdPayload.BucketListStateVariable && value == BucketListReadyState) reportsReady = true;
            if (player.Sequences.TryGetValue(variable, out var last) && !AckWindow.SequenceNewer(sequence, last)) continue;
            player.Sequences[variable] = sequence;
            switch (variable)
            {
                case NerdPayload.BucketListSlotVariable: player.Slot = unchecked((int)(uint)value); break;
                case NerdPayload.BucketListChallengeVariable: player.Challenge = unchecked((int)(uint)value); break;
                case NerdPayload.BucketListRequestingVariable: player.RequestingPartner = value != 0; break;
                case NerdPayload.BucketListStateVariable: player.State = (int)value; break;
            }
        }
        // no pairing from the flag: only a RequestSynchronize (63) asks for a partner
        if (!player.RequestingPartner) player.Unrequested = null;
        else if (player.Waiting is null && player.Pair is null && player.Unrequested != (player.Slot, player.Challenge))
        {
            player.Unrequested = (player.Slot, player.Challenge);
            Note($"bucket list: peer {peer.Index} requests a partner for slot {player.Slot}, challenge {player.Challenge} without RequestSynchronize: not paired");
        }
        if (!reportsReady || player.Pair is not { Started: false } pair) return;
        if (!create && pair.Floors.GetValueOrDefault(peer) is { } floor && !AckWindow.SequenceNewer(sequence, floor)) return;
        if (pair.Ready.Add(peer) && pair.Ready.Count == 2) StartBucketPair(pair);
    }

    /// <summary>The player deleted its own PlayerState: its wait ends (a partner would get a key to nothing); a pair stays.</summary>
    private void BucketListPlayerDeleted(RelayPeer peer)
    {
        if (!_bucketPlayers.TryGetValue(peer, out var player)) return;
        player.HasState = false; player.Latest = null; player.Sequences.Clear();
        if (player.Waiting is not null) CancelBucketWait(peer, player, "its PlayerState was deleted");
    }

    private void BucketListMessage(RelayPeer peer, NerdMessage.Other message)
    {
        var reader = new BitReader(message.Body.Bytes, message.Body.Length);
        switch (message.Kind)
        {
            case 63: RequestBucketPartner(peer, unchecked((int)(uint)reader.Read(32)), unchecked((int)(uint)reader.Read(32))); break;
            case 64: CancelBucketRequest(peer); break;
            case 62: BucketChallengeEnded(peer, reader.Boolean()); break;
        }
    }

    /// <summary>RequestSynchronize (63): the first player waiting on the same slot and challenge is the partner; else this one waits.</summary>
    private void RequestBucketPartner(RelayPeer peer, int slot, int challenge)
    {
        var player = BucketPlayerOf(peer);
        if (!player.HasState)
        {
            if (!player.EarlyNoted) Note($"bucket list: peer {peer.Index} RequestSynchronize (slot {slot}, challenge {challenge}) before its PlayerState: ignored (noted once)");
            player.EarlyNoted = true;
            return;
        }
        if (player.Waiting is { } waiting && waiting.Slot == slot && waiting.Challenge == challenge && _now - waiting.At < BucketListWaitMs
            || player.Pair is { } running && running.Slot == slot && running.Challenge == challenge)
        {
            player.PendingRequest = null; // the game's latest request is the one already served
            if (!player.RepeatNoted) Note($"bucket list: peer {peer.Index} repeats its request (slot {slot}, challenge {challenge}): ignored (noted once)");
            player.RepeatNoted = true;
            return;
        }
        if (player.LastRequestAt is { } at && _now - at < BucketListRequestMinMs)
        {
            // kept as the newest request, applied when the window passes (TickBucketList): at most one applied per window
            player.PendingRequest = (slot, challenge);
            if (!player.RateNoted) Note($"bucket list: peer {peer.Index} RequestSynchronize faster than one per {BucketListRequestMinMs} ms: the newest waits for the window (noted once)");
            player.RateNoted = true;
            return;
        }
        player.LastRequestAt = _now; player.PendingRequest = null; player.RateNoted = false; player.RepeatNoted = false; player.Unrequested = null;
        if (player.Pair is { } current) EndBucketPair(current, $"peer {peer.Index} asks for another challenge");
        if (player.Waiting is not null) CancelBucketWait(peer, player, "a new request replaces it");
        var waiter = _bucketPlayers.Where(p => p.Key != peer && p.Key.Synced && p.Value.Pair is null
                && p.Value.Waiting is { } w && w.Slot == slot && w.Challenge == challenge && _now - w.At < BucketListWaitMs)
            .OrderBy(p => p.Value.Waiting!.Value.Order).Select(p => p.Key).FirstOrDefault();
        if (waiter is not null) { PairBucketPlayers(waiter, peer, slot, challenge); return; }
        player.Waiting = (slot, challenge, _now, _bucketOrder++);
        _bucketUniverseDue.Add(peer); // a waiter is at universe 0
        ClaimBucketEntry(peer, slot, challenge);
        BucketListStatus(66, slot, challenge, peer);
        Note($"bucket list: peer {peer.Index} waits for a partner (slot {slot}, challenge {challenge})");
    }

    private void PairBucketPlayers(RelayPeer first, RelayPeer second, int slot, int challenge)
    {
        var pair = new BucketPair(first, second, slot, challenge, _now);
        foreach (var peer in new[] { first, second })
        {
            var player = BucketPlayerOf(peer);
            player.Waiting = null; player.Pair = pair; pair.Floors[peer] = player.Latest;
            _bucketUniverseDue.Add(peer); // paired during an event's phasing: back to the shared universe
        }
        if (!BucketListEntryKeptWhilePaired) FreeBucketEntries(first);
        BucketListSend(first, BucketListPartnerMessage(second, BucketListFirstStart));
        BucketListSend(second, BucketListPartnerMessage(first, BucketListSecondStart));
        BucketListStatus(67, slot, challenge, first, second);
        Note($"bucket list: peers {first.Index} and {second.Index} paired (slot {slot}, challenge {challenge}); the start waits for both at State {BucketListReadyState}");
    }

    /// <summary>65 for one partner: the other's PlayerState as a relayed key, the receiver's start position, found, the other's name.</summary>
    private BitBuffer BucketListPartnerMessage(RelayPeer partner, byte start) =>
        NerdPayload.BucketListPartner(new(HandleKind.Relay, 0, partner.Index), start, true,
            BucketListPartnerNameIsGamertag ? _bucketPlayers.GetValueOrDefault(partner)?.Name ?? string.Empty : string.Empty, BucketListNameMax);

    private void StartBucketPair(BucketPair pair)
    {
        pair.Started = true;
        var start = unchecked(NowWire + BucketListStartDelayMs);
        if (start == 0) start = 1; // 0 is "no start time"
        var message = NerdPayload.BucketListStart(start);
        BucketListSend(pair.First, message); BucketListSend(pair.Second, message);
        Note($"bucket list: peers {pair.First.Index} and {pair.Second.Index} both ready, start sent ({BucketListStartDelayMs} ms)");
    }

    /// <summary>Cancel/desync (64): ends a wait; a pair stays (the game sends one right after it is paired).</summary>
    private void CancelBucketRequest(RelayPeer peer)
    {
        if (!_bucketPlayers.TryGetValue(peer, out var player)) return;
        player.PendingRequest = null;
        if (player.Pair is not null) { Note($"bucket list: peer {peer.Index} cancel after pairing: the pair stays"); return; }
        if (player.Waiting is null) return;
        player.LastRequestAt = null; // what it asked for is over: its next 63 is taken at once
        CancelBucketWait(peer, player, "cancelled");
    }

    /// <summary>ChallengeEnded (62): the pair ends, or the wait. The player is paired again only after a new 63.</summary>
    private void BucketChallengeEnded(RelayPeer peer, bool completed)
    {
        if (!_bucketPlayers.TryGetValue(peer, out var player)) return;
        player.PendingRequest = null;
        if (player.Pair is { } pair)
        {
            // the pair is over for both partners: their next 63 is taken at once
            foreach (var partner in new[] { pair.First, pair.Second })
                if (_bucketPlayers.TryGetValue(partner, out var other)) other.LastRequestAt = null;
            EndBucketPair(pair, $"peer {peer.Index} ended its challenge ({(completed ? 1 : 0)})");
        }
        else if (player.Waiting is not null) { player.LastRequestAt = null; CancelBucketWait(peer, player, "its challenge ended"); }
    }

    private void CancelBucketWait(RelayPeer peer, BucketPlayer player, string why)
    {
        var (slot, challenge, _, _) = player.Waiting!.Value;
        player.Waiting = null;
        FreeBucketEntries(peer);
        _bucketUniverseDue.Add(peer); // settled at the flush: a player that waits again at once stays at 0
        Note($"bucket list: peer {peer.Index} stops waiting (slot {slot}, challenge {challenge}): {why}");
    }

    private void EndBucketPair(BucketPair pair, string why)
    {
        var partners = new[] { pair.First, pair.Second };
        foreach (var peer in partners)
            if (_bucketPlayers.TryGetValue(peer, out var player) && player.Pair == pair) { player.Pair = null; player.Waiting = null; }
        FreeBucketEntries(partners);
        BucketListStatus(68, pair.Slot, pair.Challenge, partners);
        // still phasing: the former partners get their private universe as everybody else has one (at the flush, so one that waits
        // again at once stays at 0)
        foreach (var peer in partners) _bucketUniverseDue.Add(peer);
        Note($"bucket list: pair of peers {pair.First.Index} and {pair.Second.Index} ended: {why}");
    }

    private void TickBucketList()
    {
        foreach (var (peer, player) in _bucketPlayers.ToArray())
        {
            if (player.Waiting is { } waiting && _now - waiting.At >= BucketListWaitMs) CancelBucketWait(peer, player, $"no partner in {BucketListWaitMs} ms");
            // the newest request that came inside the rate window, once the window has passed
            if (player.PendingRequest is { } pending && (player.LastRequestAt is not { } at || _now - at >= BucketListRequestMinMs) && _peers.Contains(peer))
            {
                player.PendingRequest = null;
                RequestBucketPartner(peer, pending.Slot, pending.Challenge);
            }
        }
        foreach (var pair in _bucketPlayers.Values.Select(p => p.Pair).OfType<BucketPair>().Distinct().ToArray())
        {
            if (pair.Started || pair.StallNoted || _now - pair.PairedAt < BucketListStartStallMs) continue;
            pair.StallNoted = true;
            var partners = new[] { pair.First, pair.Second }.Select(p =>
                $"peer {p.Index} {(pair.Ready.Contains(p) ? "ready" : "not ready")} (last State {_bucketPlayers.GetValueOrDefault(p)?.State})");
            Note($"bucket list: pair of peers {pair.First.Index} and {pair.Second.Index} not started {BucketListStartStallMs} ms after pairing: {string.Join(", ", partners)}");
        }
        FlushBucketList();
    }

    private void BucketListPeerLeft(RelayPeer peer)
    {
        if (!_bucketPlayers.Remove(peer, out var player)) return;
        if (player.Pair is { } pair) EndBucketPair(pair, $"peer {peer.Index} left");
        else if (player.Waiting is not null) { player.Waiting = null; FreeBucketEntries(peer); }
        FlushBucketList();
    }

    private void ClearBucketList()
    {
        _bucketPlayers.Clear(); _bucketOutbox.Clear(); _bucketUniverseDue.Clear();
        Array.Clear(_bucketSlots); Array.Clear(_bucketSlotsSent);
    }
}

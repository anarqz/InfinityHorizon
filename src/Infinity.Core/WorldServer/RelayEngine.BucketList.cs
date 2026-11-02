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
    /// <summary>
    /// A PlayerState raising RequestingPartner (with its CurrentSlotId/CurrentChallengeId) is a request when no 63 from that player comes
    /// within this long (live, 2026-11-02: the game never sent 63; START and JOIN both only raised the flag).
    /// </summary>
    internal const long BucketListRequestGraceMs = 800;
    /// <summary>A pair not started this long after its pairing is logged once (V11 diagnostic: who has not reported State 2).</summary>
    internal const long BucketListStartStallMs = 15000;
    /// <summary>
    /// The PlayerStates that make a partner ready for 61 when an update after the pairing reports them (the game's
    /// BucketListPlayerStateEnum: 1 Loading, 2 Ready, 3 Active, 4 Finished, 5 Retry): 2 Ready, and 3 Active, which only follows Ready
    /// (a game past Ready whose Ready report was folded into a later one); never 4 Finished or 5 Retry. The reference capture showed
    /// State 1, then 2, before its 61. Live test.3 saw no State past 0 because State is 4 bits, not 3: every update carrying it failed
    /// the sanitizer, so it was neither read nor relayed to the partner.
    /// </summary>
    internal const int BucketListStateReady = 2, BucketListStateActive = 3;
    private static bool BucketListReady(ulong state) => state is BucketListStateReady or BucketListStateActive;
    /// <summary>
    /// A last resort, unverified: when both partners never report Ready, 61 still goes this long after both partners' cancel/desync (64,
    /// sent right after the game took its 65 and its own RequestChallenge, so 61 is not wiped), before the games' own ~15 s timeout ends a
    /// pair that would otherwise never start. V11 decides whether it ever fires.
    /// </summary>
    internal const long BucketListStartFallbackMs = 10000;
    /// <summary>Diagnostic lines per player and wait or pair (decoded PlayerState updates) and per player (sanitizer refusals).</summary>
    internal const int BucketListTraceLines = 12;
    internal const int BucketListNameMax = 36;
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
        /// <summary>When an update raised RequestingPartner (or moved the slot/challenge under it): a request once the grace passes.</summary>
        internal long? FlagRequestAt { get; set; }
        /// <summary>When the player's last RequestSynchronize (63) arrived: within the grace it wins over the flag, until a 64, a 62 or a
        /// fallen flag ends what it asked for.</summary>
        internal long? Last63At { get; set; }
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
        /// <summary>Decoded PlayerState lines left for this wait or pair (V11 diagnostic).</summary>
        internal int Trace { get; set; }
        /// <summary>Sanitizer refusal lines left for this player (V11 diagnostic).</summary>
        internal int RefusedTrace { get; set; } = BucketListTraceLines;
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
        /// <summary>The partners whose cancel/desync (64) came after the pairing, and when the second one did.</summary>
        internal HashSet<RelayPeer> Cancelled { get; } = [];
        internal long? CancelledAt { get; set; }
        /// <summary>The partners whose RequestingPartner fell to 0 after the pairing: both of them backed out, so no fallback start.</summary>
        internal HashSet<RelayPeer> FlagFell { get; } = [];
        /// <summary>The pair's bucket-list universe (65's 5-bit value, the same to both): freed when the pair ends.</summary>
        internal byte Universe { get; init; }
        /// <summary>Each partner's last car prediction when 61 went (V11: the first one after it shows the universe the car is in).</summary>
        internal Dictionary<RelayPeer, NerdMessage.Car?> CarsAtStart { get; } = [];
        internal bool CarsNoted { get; set; }
    }

    private readonly Dictionary<RelayPeer, BucketPlayer> _bucketPlayers = [];
    private readonly (int Slot, int Challenge, int Owner)[] _bucketSlots = new (int, int, int)[BucketListEntries];
    /// <summary>The slot array as the players have it (the last delta sent): joiners get this, at its sequence.</summary>
    private readonly (int Slot, int Challenge, int Owner)[] _bucketSlotsSent = new (int, int, int)[BucketListEntries];
    private readonly List<(RelayPeer To, BitBuffer Message)> _bucketOutbox = [];
    private byte _bucketSlotsSequence;
    private long _bucketOrder;
    /// <summary>When each pair universe was last freed (a pair ended).</summary>
    private readonly Dictionary<byte, long> _bucketUniversesFreed = [];
    /// <summary>
    /// A freed pair universe is not handed to another pair for this long when another is free: the former partners' games may still be
    /// in it (and their cars report it) until they clear their override and send their next prediction.
    /// </summary>
    internal const long BucketListUniverseCoolMs = 30000;

    /// <summary>Bucket-list co-op is free roam's: no car meet or Road Trip world serves it. Not behind <see cref="RelaySettings.Races"/>.</summary>
    private bool BucketListWorld => _settings.CarMeetLobby is null && !IsRoadTrip;

    /// <summary>A waiting or paired player keeps universe 0 (no private universe for it while an event phases the others).</summary>
    private bool BucketListBusy(RelayPeer peer) => _bucketPlayers.TryGetValue(peer, out var player) && (player.Waiting is not null || player.Pair is not null);

    /// <summary>Whether a running pair holds this universe (event phasing never hands it out meanwhile).</summary>
    private bool BucketListUniverseHeld(byte universe) => universe != 0 && _bucketPlayers.Values.Any(p => p.Pair?.Universe == universe);

    /// <summary>
    /// A paired player's car universe for the relay's phasing (its pair's, which the game enters itself for the challenge); null when not
    /// paired. The player's PrivateUniverseId stays 0: the game's bucket-list override, not var 40, moves the car.
    /// </summary>
    private byte? BucketListPairUniverse(RelayPeer peer) => _bucketPlayers.GetValueOrDefault(peer)?.Pair is { Universe: not 0 } pair ? pair.Universe : null;

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

    /// <summary>
    /// Fail open for the player's own PlayerState (live test.3: a layout mismatch refused every State update, so neither game ever saw
    /// its partner Ready): a payload the pinned table cannot read fully still passes on its clean prefix (the variables before the first
    /// unknown or repeated id, cut value or State past the enum), re-encoded, within its size bound and only from its owner; the raw bits
    /// never pass. Nothing decodes: dropped. False: every such payload is dropped, as for routers 1-3.
    /// </summary>
    internal const bool BucketListPlayerFailOpen = true;

    /// <summary>
    /// A PlayerState the sanitizer refused (V11: an id order or width other than the table's shows here), bounded per player. The clean
    /// prefix to relay instead (<see cref="BucketListPlayerFailOpen"/>), or null: dropped.
    /// </summary>
    private BitBuffer? BucketListSanitizeRefused(RelayPeer peer, NerdMessage.Object obj, PayloadFailure? fault)
    {
        var prefix = BucketListPlayerFailOpen && obj.Payload.Length <= NerdPayload.BucketListPlayerMaxBits
            && NerdPayload.BucketListPlayerOf(obj.Payload) is { Count: > 0 } clean ? NerdPayload.BucketListPlayer(clean) : null;
        var player = BucketPlayerOf(peer);
        if (player.RefusedTrace > 0)
        {
            player.RefusedTrace--;
            // the hex is safe to log: a PlayerState payload (at most 128 bits, router 12 only) holds numbers and flags (Score, slot,
            // challenge, State), never a name, XUID, address or player text
            Note($"bucket list: peer {peer.Index} PlayerState {(obj.Create ? "create" : "update")} seq {obj.Sequence} refused by the sanitizer: {fault?.Fault} {fault?.Value}, {obj.Payload.Length} bits {Convert.ToHexString(obj.Payload.Bytes)}{(prefix is not null ? $"; its clean prefix relayed ({prefix.Length} bits, fail open)" : "; dropped")}");
        }
        return prefix;
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
        var reportsReady = false; // this update itself carries State Ready or Active (a remembered State never counts)
        var (wasRequesting, oldSlot, oldChallenge) = (player.RequestingPartner, player.Slot, player.Challenge);
        var raises = false; // this update itself sets RequestingPartner 1 (a re-sent 1 after a cancel is a new request)
        var variables = NerdPayload.BucketListPlayerOf(payload);
        if ((player.Waiting is not null || player.Pair is not null) && player.Trace > 0)
        {
            player.Trace--; // V11: what the game reports while it waits or plays co-op
            Note($"bucket list: trace peer {peer.Index} PlayerState {(create ? "create" : "update")} seq {sequence}{(player.Pair is not null ? " (paired)" : " (waiting)")}: {string.Join(" ", variables.Select(v => $"{v.Variable}={v.Value}"))}");
        }
        foreach (var (variable, value) in variables)
        {
            if (variable == NerdPayload.BucketListStateVariable && BucketListReady(value)) reportsReady = true;
            if (player.Sequences.TryGetValue(variable, out var last) && !AckWindow.SequenceNewer(sequence, last)) continue;
            player.Sequences[variable] = sequence;
            switch (variable)
            {
                case NerdPayload.BucketListSlotVariable: player.Slot = unchecked((int)(uint)value); break;
                case NerdPayload.BucketListChallengeVariable: player.Challenge = unchecked((int)(uint)value); break;
                case NerdPayload.BucketListRequestingVariable: player.RequestingPartner = value != 0; raises |= value != 0; break;
                case NerdPayload.BucketListStateVariable: player.State = (int)value; break;
            }
        }
        if (player.RequestingPartner && (raises || player.Slot != oldSlot || player.Challenge != oldChallenge)) BucketListFlagRaised(player);
        else if (wasRequesting && !player.RequestingPartner) BucketListFlagDropped(peer, player);
        if (!reportsReady || player.Pair is not { Started: false } pair) return;
        if (!create && pair.Floors.GetValueOrDefault(peer) is { } floor && !AckWindow.SequenceNewer(sequence, floor)) return;
        if (pair.Ready.Add(peer) && pair.Ready.Count == 2) StartBucketPair(pair);
    }

    /// <summary>
    /// RequestingPartner raised (or the slot/challenge moved under it): a request for the current slot and challenge once
    /// <see cref="BucketListRequestGraceMs"/> passes without a 63 (TickBucketList). Nothing for what is already being served.
    /// </summary>
    private void BucketListFlagRaised(BucketPlayer player)
    {
        if (player.Waiting is { } waiting && waiting.Slot == player.Slot && waiting.Challenge == player.Challenge && _now - waiting.At < BucketListWaitMs
            || player.Pair is { } pair && pair.Slot == player.Slot && pair.Challenge == player.Challenge)
        { player.FlagRequestAt = null; return; }
        if (player.Last63At is { } at && _now - at < BucketListRequestGraceMs) return; // a 63 came: it wins
        player.FlagRequestAt ??= _now; // a move inside the grace keeps its deadline; the values are read when it is applied
    }

    /// <summary>RequestingPartner fell to 0: a pending flag request is dropped; before a pair the wait ends (as 64); a pair stays.</summary>
    private void BucketListFlagDropped(RelayPeer peer, BucketPlayer player)
    {
        player.FlagRequestAt = null;
        if (player.Pair is { } pair) { pair.FlagFell.Add(peer); return; }
        if (player.Waiting is null) return;
        player.LastRequestAt = null; player.Last63At = null; player.PendingRequest = null; // its next request is taken at once
        CancelBucketWait(peer, player, "RequestingPartner fell to 0");
    }

    /// <summary>The player deleted its own PlayerState: its wait ends (a partner would get a key to nothing); a pair stays.</summary>
    private void BucketListPlayerDeleted(RelayPeer peer)
    {
        if (!_bucketPlayers.TryGetValue(peer, out var player)) return;
        player.HasState = false; player.Latest = null; player.Sequences.Clear(); player.FlagRequestAt = null;
        player.RequestingPartner = false; player.Slot = 0; player.Challenge = 0;
        if (player.Waiting is not null) CancelBucketWait(peer, player, "its PlayerState was deleted");
    }

    private void BucketListMessage(RelayPeer peer, NerdMessage.Other message)
    {
        var reader = new BitReader(message.Body.Bytes, message.Body.Length);
        switch (message.Kind)
        {
            case 63:
                var player = BucketPlayerOf(peer);
                player.Last63At = _now; player.FlagRequestAt = null; // the 63 wins over a flag request inside the grace
                RequestBucketPartner(peer, unchecked((int)(uint)reader.Read(32)), unchecked((int)(uint)reader.Read(32)));
                break;
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
        player.LastRequestAt = _now; player.PendingRequest = null; player.RateNoted = false; player.RepeatNoted = false;
        if (player.Pair is { } current) EndBucketPair(current, $"peer {peer.Index} asks for another challenge");
        if (player.Waiting is not null) CancelBucketWait(peer, player, "a new request replaces it");
        var waiter = _bucketPlayers.Where(p => p.Key != peer && p.Key.Synced && p.Value.Pair is null
                && p.Value.Waiting is { } w && w.Slot == slot && w.Challenge == challenge && _now - w.At < BucketListWaitMs)
            .OrderBy(p => p.Value.Waiting!.Value.Order).Select(p => p.Key).FirstOrDefault();
        if (waiter is not null) { PairBucketPlayers(waiter, peer, slot, challenge); return; }
        player.Waiting = (slot, challenge, _now, _bucketOrder++);
        player.Trace = BucketListTraceLines;
        _bucketUniverseDue.Add(peer); // a waiter is at universe 0
        ClaimBucketEntry(peer, slot, challenge);
        BucketListStatus(66, slot, challenge, peer);
        Note($"bucket list: peer {peer.Index} waits for a partner (slot {slot}, challenge {challenge})");
    }

    private void PairBucketPlayers(RelayPeer first, RelayPeer second, int slot, int challenge)
    {
        var pair = new BucketPair(first, second, slot, challenge, _now) { Universe = BucketListPickUniverse() };
        foreach (var peer in new[] { first, second })
        {
            var player = BucketPlayerOf(peer);
            player.Waiting = null; player.Pair = pair; pair.Floors[peer] = player.Latest; player.Trace = BucketListTraceLines;
            _bucketUniverseDue.Add(peer); // the relay's own phasing keeps the partners at 0: the game moves them to the pair's universe
        }
        if (!BucketListEntryKeptWhilePaired) FreeBucketEntries(first);
        foreach (var (receiver, partner) in new[] { (first, second), (second, first) })
        {
            var name = BucketListPartnerName(partner);
            var message = BucketListPartnerMessage(partner, pair.Universe, name);
            BucketListSend(receiver, message);
            // V11: what each game gets, and whether the PlayerState it names has reached it (relayed live and in join snapshots; routers 12
            // are never held for a joiner). No bytes: 65 carries the partner's name, and the log carries no names, only its length
            var known = partner.Objects.TryGetValue((BucketListRouter, 0), out var state) ? $"yes (its sequence {state.Last})" : "no";
            Note($"bucket list: 65 to peer {receiver.Index}: {message.Length} bits; partner peer {partner.Index}, universe {pair.Universe}, name of {Math.Min(name.Length, BucketListNameMax)} characters; partner PlayerState known to it: {known}");
        }
        BucketListStatus(67, slot, challenge, first, second);
        Note($"bucket list: peers {first.Index} and {second.Index} paired (slot {slot}, challenge {challenge}) in universe {pair.Universe}; the start waits for both at State {BucketListStateReady}/{BucketListStateActive}, or {BucketListStartFallbackMs} ms after both cancels");
    }

    /// <summary>
    /// The pair's bucket-list universe: the highest OnlinePrivate universe (<see cref="LastPrivateUniverse"/> down to
    /// <see cref="FirstPrivateUniverse"/>) that no player holds for event phasing, no running pair uses and no pair left in the last
    /// <see cref="BucketListUniverseCoolMs"/> (when all free ones are that fresh, the least recently freed). Event phasing takes the lowest
    /// free one for each player not waiting or paired, so with P pairs it holds at most 12 - 2P universes (1 to 12 - 2P) while the pairs
    /// hold 17 down to 18 - P: the two never meet. 0 (free roam) only if all are taken, which 12 players cannot reach.
    /// </summary>
    private byte BucketListPickUniverse()
    {
        var taken = _bucketPlayers.Values.Select(p => p.Pair?.Universe ?? 0).Concat(_peers.Select(p => p.Universe)).ToHashSet();
        var free = Enumerable.Range(FirstPrivateUniverse, LastPrivateUniverse - FirstPrivateUniverse + 1).Select(u => (byte)u)
            .Where(u => !taken.Contains(u)).OrderByDescending(u => u).ToList();
        // a universe a pair just left stays out a while: its cars may still report it until their games leave it
        bool Cooling(byte u) => _bucketUniversesFreed.TryGetValue(u, out var at) && _now - at < BucketListUniverseCoolMs;
        if (free.FirstOrDefault(u => !Cooling(u)) is var rested and not 0) return rested;
        if (free.Count != 0) return free.MinBy(u => _bucketUniversesFreed.GetValueOrDefault(u)); // all cooling: the least recently freed
        Note("bucket list: no free private universe for a pair, it stays in free roam (0)");
        return 0;
    }

    /// <summary>
    /// 65 for one partner: the other's PlayerState as a relayed key, the pair's bucket-list universe (the same for both: the game stores it
    /// in its bucket-list component and enters it through its ToBucketListUniverse network override for the challenge, clearing it at the
    /// end), found, the other's name.
    /// </summary>
    private static BitBuffer BucketListPartnerMessage(RelayPeer partner, byte universe, string name) =>
        NerdPayload.BucketListPartner(new(HandleKind.Relay, 0, partner.Index), universe, true, name, BucketListNameMax);

    /// <summary>The name 65 gives a partner (never logged).</summary>
    private string BucketListPartnerName(RelayPeer partner) =>
        BucketListPartnerNameIsGamertag ? _bucketPlayers.GetValueOrDefault(partner)?.Name ?? string.Empty : string.Empty;

    /// <summary>The universe a car prediction says its car is in (CarPredictionFull bits 395-399); null without one.</summary>
    private static ulong? CarUniverseOf(NerdMessage.Car? car)
    {
        if (car is null || car.Body.Length < CarUniverseOffset + PrivateUniverseBits) return null;
        var reader = new BitReader(car.Body.Bytes, car.Body.Length);
        reader.Skip(CarUniverseOffset);
        return reader.Read(PrivateUniverseBits);
    }

    /// <summary>V11: the first car prediction of each partner after 61, with the universe it reports (once per partner and pair).</summary>
    private void TraceBucketPairCars(BucketPair pair)
    {
        foreach (var (peer, atStart) in pair.CarsAtStart.ToArray())
        {
            if (ReferenceEquals(peer.LastCar, atStart) || peer.LastCar is null) continue;
            pair.CarsAtStart.Remove(peer);
            Note($"bucket list: peer {peer.Index}'s car after the start reports universe {CarUniverseOf(peer.LastCar)} (the pair's is {pair.Universe})");
        }
    }

    private void StartBucketPair(BucketPair pair, bool fallback = false)
    {
        pair.Started = true;
        foreach (var peer in new[] { pair.First, pair.Second }) pair.CarsAtStart[peer] = peer.LastCar;
        var start = unchecked(NowWire + BucketListStartDelayMs);
        if (start == 0) start = 1; // 0 is "no start time"
        var message = NerdPayload.BucketListStart(start);
        BucketListSend(pair.First, message); BucketListSend(pair.Second, message);
        Note($"bucket list: peers {pair.First.Index} and {pair.Second.Index} {(fallback ? "by fallback" : "both ready")}, start sent ({BucketListStartDelayMs} ms)");
    }

    /// <summary>
    /// The pair still runs on both sides: both partners here, both still in it, and not both with RequestingPartner fallen since the
    /// pairing (a 62 or a leave ends the pair itself).
    /// </summary>
    private bool BucketPairLive(BucketPair pair) => new[] { pair.First, pair.Second }.All(p => _peers.Contains(p)
        && _bucketPlayers.GetValueOrDefault(p)?.Pair == pair) && pair.FlagFell.Count < 2;

    /// <summary>Cancel/desync (64): ends a wait; a pair stays (the game sends one right after it is paired).</summary>
    private void CancelBucketRequest(RelayPeer peer)
    {
        if (!_bucketPlayers.TryGetValue(peer, out var player)) return;
        player.PendingRequest = null; player.FlagRequestAt = null; // a cancel inside the grace also ends the flag's request
        if (player.Pair is { } pair)
        {
            // the game's own cancel/desync right after taking its 65: the pair stays; both of them arm the start fallback
            if (pair.Cancelled.Add(peer) && pair.Cancelled.Count == 2) pair.CancelledAt = _now;
            Note($"bucket list: peer {peer.Index} cancel after pairing: the pair stays");
            return;
        }
        if (player.Waiting is null) return;
        player.LastRequestAt = null; player.Last63At = null; // what it asked for is over: its next request is taken at once
        CancelBucketWait(peer, player, "cancelled");
    }

    /// <summary>ChallengeEnded (62): the pair ends, or the wait. The player is paired again only after a new 63.</summary>
    private void BucketChallengeEnded(RelayPeer peer, bool completed)
    {
        if (!_bucketPlayers.TryGetValue(peer, out var player)) return;
        player.PendingRequest = null; player.FlagRequestAt = null;
        if (player.Pair is { } pair)
        {
            // the pair is over for both partners: their next 63 is taken at once
            foreach (var partner in new[] { pair.First, pair.Second })
                if (_bucketPlayers.TryGetValue(partner, out var other)) { other.LastRequestAt = null; other.Last63At = null; }
            EndBucketPair(pair, $"peer {peer.Index} ended its challenge ({(completed ? 1 : 0)})");
        }
        else if (player.Waiting is not null) { player.LastRequestAt = null; player.Last63At = null; CancelBucketWait(peer, player, "its challenge ended"); }
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
        if (pair.Universe != 0) _bucketUniversesFreed[pair.Universe] = _now;
        Note($"bucket list: pair of peers {pair.First.Index} and {pair.Second.Index} ended: {why}; universe {pair.Universe} freed{(pair.Started ? $", its cars last reported universe {CarUniverseOf(pair.First.LastCar)}/{CarUniverseOf(pair.Second.LastCar)}" : string.Empty)}");
    }

    private void TickBucketList()
    {
        foreach (var (peer, player) in _bucketPlayers.ToArray())
        {
            if (player.Waiting is { } waiting && _now - waiting.At >= BucketListWaitMs) CancelBucketWait(peer, player, $"no partner in {BucketListWaitMs} ms");
            // RequestingPartner raised and no 63 within the grace: the flag is the request, for the slot and challenge it has now
            if (player.FlagRequestAt is { } raised && _now - raised >= BucketListRequestGraceMs)
            {
                player.FlagRequestAt = null;
                // no challenge selected (slot 0, challenge 0) is no request, whatever the flag says
                if (player.RequestingPartner && _peers.Contains(peer) && (player.Slot != 0 || player.Challenge != 0))
                {
                    Note($"bucket list: peer {peer.Index} request from PlayerState (no 63 within {BucketListRequestGraceMs} ms): slot {player.Slot}, challenge {player.Challenge}");
                    RequestBucketPartner(peer, player.Slot, player.Challenge);
                }
            }
            // the newest request that came inside the rate window, once the window has passed
            if (player.PendingRequest is { } pending && (player.LastRequestAt is not { } at || _now - at >= BucketListRequestMinMs) && _peers.Contains(peer))
            {
                player.PendingRequest = null;
                RequestBucketPartner(peer, pending.Slot, pending.Challenge);
            }
        }
        foreach (var pair in _bucketPlayers.Values.Select(p => p.Pair).OfType<BucketPair>().Distinct().ToArray())
        {
            if (!pair.Started && pair.CancelledAt is { } cancelled && _now - cancelled >= BucketListStartFallbackMs && BucketPairLive(pair))
            {
                Note($"bucket list: peers {pair.First.Index} and {pair.Second.Index}: STARTED WITHOUT BOTH READY (start by fallback, {BucketListStartFallbackMs} ms after both cancels) (ready: {string.Join(",", pair.Ready.Select(p => p.Index))}; last States {_bucketPlayers.GetValueOrDefault(pair.First)?.State}/{_bucketPlayers.GetValueOrDefault(pair.Second)?.State})");
                StartBucketPair(pair, fallback: true);
            }
            // a log line only: nothing is sent, the pair stays (the games' own timeout ends it)
            if (pair.Started) TraceBucketPairCars(pair);
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
        _bucketPlayers.Clear(); _bucketOutbox.Clear(); _bucketUniverseDue.Clear(); _bucketUniversesFreed.Clear();
        Array.Clear(_bucketSlots); Array.Clear(_bucketSlotsSent);
    }
}

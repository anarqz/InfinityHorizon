// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.WorldServer;

internal enum GameOptionsUpdateResult { Ignored, Empty, Stored, Buffered, Overflow, TooLong }

/// <summary>
/// A bounded journal of original GameOptions updates, replayed individually. After the first arbitrary source sequence, gaps
/// must close before any later fragment is published. The journal outlives events (the owner's game writes only deltas), so
/// <see cref="Compact"/> rewrites it, when every record reads (GameOptionsFields), as the writes of its merged values.
/// Real-client acceptance of ownership replay remains a compatibility gate.
/// </summary>
internal sealed class RelayGameOptionsState
{
    internal const int MaxBits = 64 * 1024;
    // A replay spans less than the modulo-255 sequence half-window, including its grant.
    internal const int MaxRecords = 127;
    /// <summary>
    /// The relayed prefix of a record, as an update (8) or a grant (11) alike: 7 id + 4 router + 2 kind + 2 id (router 6's key, a
    /// transfer key: no owner byte) + 8 sequence + 14 length.
    /// </summary>
    internal const int RelayedPrefixBits = 37;
    /// <summary>The longest record kept: one whose relayed update or grant fits one datagram (and so whatever the game itself can send).</summary>
    internal const int MaxRecordBits = NerdCodec.MaxMessageBits - RelayedPrefixBits;
    private readonly List<BitBuffer> _records = [];
    private readonly Dictionary<byte, (BitBuffer Payload, long At)> _pending = [];
    internal uint? ObjectId { get; private set; }
    internal RelayPeer? SourceOwner { get; private set; }
    internal byte? SourceSequence { get; private set; }
    internal int Bits { get; private set; }
    internal int PendingBits { get; private set; }
    internal int PendingCount => _pending.Count;
    private long? GapSince { get; set; }
    internal IReadOnlyList<BitBuffer> Records => _records;

    internal void ChangeOwner(RelayPeer owner)
    {
        if (ReferenceEquals(SourceOwner, owner)) return;
        SourceOwner = owner;
        SourceSequence = null;
        // Pending fragments were never published; the next authority inherits the accepted prefix.
        ClearPending();
    }

    internal void BeginSourceEpoch(RelayPeer owner)
    {
        // A valid ownership grant may change the client's outgoing counter even when the
        // same connection retains authority. Its first post-grant sequence is not assumed.
        SourceOwner = owner;
        SourceSequence = null;
        ClearPending();
    }

    internal GameOptionsUpdateResult Apply(RelayPeer owner, uint id, byte sequence, BitBuffer payload)
        => Apply(owner, id, sequence, payload, 0, out _);

    internal GameOptionsUpdateResult Apply(RelayPeer owner, uint id, byte sequence, BitBuffer payload,
        long nowMilliseconds, out IReadOnlyList<BitBuffer> ready)
    {
        ready = [];
        if (sequence == byte.MaxValue) return GameOptionsUpdateResult.Ignored;
        ChangeOwner(owner);
        if (ObjectId != id)
        {
            ObjectId = id;
            SourceSequence = null;
            _records.Clear();
            Bits = 0;
            ClearPending();
        }
        if (_pending.ContainsKey(sequence) || SourceSequence is { } last && !AckWindow.SequenceNewer(sequence, last))
            return GameOptionsUpdateResult.Ignored;
        // a record must fit one datagram once relayed (as an update or a grant, under any key): refused before it is stored, so no
        // receiver's sequence is ever spent on a message that cannot go out (NERDCORE_WIRE.md §2.3)
        if (payload.Length > MaxRecordBits) return GameOptionsUpdateResult.TooLong;
        var future = SourceSequence is { } previous && sequence != AckWindow.SequenceNext(previous);
        if (payload.Length > NerdCodec.MaxPayloadBits || payload.Length > MaxBits - Bits - PendingBits
            || (future || payload.Length != 0) && _records.Count + _pending.Count >= MaxRecords)
            return GameOptionsUpdateResult.Overflow;
        var fragment = BitBuffer.From(payload.Bytes, payload.Length);
        if (future)
        {
            _pending.Add(sequence, (fragment, nowMilliseconds));
            PendingBits += fragment.Length;
            GapSince ??= nowMilliseconds;
            return GameOptionsUpdateResult.Buffered;
        }
        var accepted = new List<BitBuffer>();
        Commit(sequence, fragment, accepted);
        while (_pending.Remove(AckWindow.SequenceNext(SourceSequence!.Value), out var next))
        {
            PendingBits -= next.Payload.Length;
            Commit(AckWindow.SequenceNext(SourceSequence.Value), next.Payload, accepted);
        }
        GapSince = _pending.Count == 0 ? null : _pending.Values.Min(p => p.At);
        ready = accepted;
        return accepted.Count == 0 ? GameOptionsUpdateResult.Empty : GameOptionsUpdateResult.Stored;
    }

    private void Commit(byte sequence, BitBuffer fragment, List<BitBuffer> ready)
    {
        // Empty records advance ordering but never accumulate in replay history or on the wire.
        SourceSequence = sequence;
        if (fragment.Length == 0) return;
        _records.Add(fragment);
        Bits += fragment.Length;
        ready.Add(fragment);
    }

    /// <summary>
    /// Rewrites the accepted records as the game's own writes of their merged values (the last write of each variable wins): ids
    /// ascending, each record no longer than the longest one the game wrote (so it fits the packets the game's own did), so
    /// replaying them sets exactly what replaying the old ones did. Pending fragments and the source sequence stay. False,
    /// changing nothing, when a record does not read.
    /// </summary>
    internal bool Compact()
    {
        if (_records.Count == 0) return true;
        if (!GameOptionsFields.TryRead(_records, out var fields)) return false;
        // every variable came in one of the records, so each one fits a record on its own
        var longest = _records.Max(r => r.Length);
        var compacted = new List<BitBuffer>();
        var record = new BitBuffer();
        foreach (var (id, raw) in fields.Values)
        {
            if (record.Length != 0 && record.Length + GameOptionsFields.IdBits + raw.Length > longest)
            {
                compacted.Add(record);
                record = new();
            }
            record.Push((ulong)id, GameOptionsFields.IdBits).Append(raw);
        }
        if (record.Length != 0) compacted.Add(record);
        _records.Clear();
        _records.AddRange(compacted);
        Bits = compacted.Sum(r => r.Length);
        return true;
    }

    internal RelayPeer? ExpiredSource(long nowMilliseconds, long timeoutMilliseconds) =>
        GapSince is { } since && nowMilliseconds >= since && nowMilliseconds - since >= timeoutMilliseconds ? SourceOwner : null;

    private void ClearPending()
    {
        _pending.Clear();
        PendingBits = 0;
        GapSince = null;
    }

    internal void Clear()
    {
        ObjectId = null;
        SourceOwner = null;
        SourceSequence = null;
        Bits = 0;
        _records.Clear();
        ClearPending();
    }
}

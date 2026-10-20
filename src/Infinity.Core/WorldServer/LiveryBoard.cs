// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.WorldServer;

internal sealed record LiveryOutput(ulong To, LiveryMessage Message);
internal abstract record LiveryNote
{
    internal sealed record Stored(ulong Xuid, int Bytes) : LiveryNote;
    internal sealed record Refused(ulong Xuid, string Reason) : LiveryNote;
    internal sealed record Skipped(ulong Owner, ulong To) : LiveryNote;
}

/// <summary>One world's in-memory livery synchronization. The owner supplies monotonic milliseconds.</summary>
internal sealed class LiveryBoard
{
    internal const long ResendEveryMs = 500;
    internal const int ResendMax = 20;
    internal const long PutEveryMs = 1000;
    internal const long WantEveryMs = 1000;
    internal const long PartialTtlMs = 5000;
    internal const long TransferTtlMs = 120000;
    internal const long OutputWindowMs = 20;
    internal const int MaxOutputDatagrams = 128;
    internal const int MaxOutputWireBytes = 64 * 1024;
    internal const int MaxControlDatagrams = 16;
    internal const int MaxParticipants = 12;
    internal const int MaxTransfers = MaxParticipants * (MaxParticipants - 1);
    private const int MaxControls = 256;
    private const int MaxNotes = 64;

    private sealed record Livery(ushort Sequence, LiveryMeta Meta, byte[] Bytes);
    private sealed class Partial(LiveryChunk chunk, long now)
    {
        internal ushort Sequence { get; } = chunk.Sequence;
        internal LiveryMeta Meta { get; } = chunk.Meta;
        internal uint Total { get; } = chunk.Total;
        internal byte[]?[] Parts { get; } = new byte[chunk.Count][];
        internal int Got { get; set; }
        internal long Started { get; } = now;
    }
    private sealed class Pending(ushort sequence, long now)
    {
        internal ushort Sequence { get; } = sequence;
        internal long Created { get; } = now;
        internal long Sent { get; set; }
        internal int Tries { get; set; }
        internal int NextChunk { get; set; }
        internal bool Sending { get; set; } = true;
    }
    private sealed class Viewer
    {
        internal Dictionary<ulong, Pending> Pending { get; } = [];
        internal HashSet<ulong>? SyncWait { get; set; }
        internal byte SyncCount { get; set; }
        internal long? WantAt { get; set; }
        internal int Generation { get; set; }
    }
    private readonly record struct Flow(ulong To, ulong Owner);
    private sealed record Control(LiveryOutput Output, int Generation = 0);

    private readonly HashSet<ulong> _participants = [];
    private readonly Dictionary<ulong, Livery> _liveries = [];
    private readonly Dictionary<ulong, Partial> _partials = [];
    private readonly Dictionary<ulong, long> _newSequenceAt = [];
    private readonly Dictionary<ulong, Viewer> _viewers = [];
    private readonly List<Flow> _flows = [];
    private readonly Queue<Control> _controls = [];
    private readonly List<LiveryNote> _notes = [];
    private int _nextFlow, _windowControls;
    private long _now, _windowAt = long.MinValue;

    // Both ends are checked before generating a chunk, not merely before socket transmission.
    internal Func<ulong, bool>? Authorize { get; set; }
    internal int Stored => _liveries.Count;
    internal int PendingTransfers => _flows.Count;
    internal int QueuedControls => _controls.Count;
    internal int WindowDatagrams { get; private set; }
    internal int WindowWireBytes { get; private set; }
    internal int BytesHeld => _liveries.Values.Sum(l => l.Bytes.Length)
        + _partials.Values.Sum(p => p.Parts.Sum(part => part?.Length ?? 0));

    internal IReadOnlyList<LiveryNote> TakeNotes()
    {
        var result = _notes.ToArray(); _notes.Clear(); return result;
    }

    private void Note(LiveryNote note)
    {
        if (_notes.Count == MaxNotes) _notes.RemoveAt(0);
        _notes.Add(note);
    }

    internal static bool SequenceAfter(ushort next, ushort previous) => next != previous && unchecked((ushort)(next - previous)) < 0x8000;
    private bool Allowed(ulong xuid) => Authorize?.Invoke(xuid) ?? true;
    private bool Register(ulong xuid) => _participants.Contains(xuid) || _participants.Count < MaxParticipants && _participants.Add(xuid);

    private void Advance(long now)
    {
        _now = Math.Max(_now, now);
        if (_windowAt == long.MinValue || _now - _windowAt >= OutputWindowMs)
        { _windowAt = _now; WindowDatagrams = 0; WindowWireBytes = 0; _windowControls = 0; }
        foreach (var xuid in _participants.Where(xuid => !Allowed(xuid)).ToArray()) RemovePeer(xuid);
    }

    private void QueueControl(ulong to, LiveryMessage message, int generation = 0)
    {
        var control = new Control(new(to, message), generation);
        if (_controls.Count < MaxControls && !_controls.Contains(control)) _controls.Enqueue(control);
    }

    private void StartTransfer(ulong to, Viewer viewer, ulong owner, ushort sequence)
    {
        if (!viewer.Pending.ContainsKey(owner)) _flows.Add(new(to, owner));
        viewer.Pending[owner] = new(sequence, _now);
    }

    private void RemoveTransfer(ulong to, Viewer viewer, ulong owner)
    {
        viewer.Pending.Remove(owner);
        var at = _flows.IndexOf(new(to, owner));
        if (at < 0) return;
        _flows.RemoveAt(at);
        if (at < _nextFlow) _nextFlow--;
        if (_nextFlow >= _flows.Count) _nextFlow = 0;
    }

    internal List<LiveryOutput> OnPut(ulong from, LiveryChunk chunk, long now)
    {
        Advance(now);
        if (!Allowed(from)) return [];
        if (chunk.Owner != from) { Note(new LiveryNote.Refused(from, "another peer's livery")); return []; }
        // Parsed network chunks satisfy this too; do not trust in-process callers with allocations.
        var count = Math.Max(1, ((long)chunk.Total + LiveryCodec.ChunkBytes - 1) / LiveryCodec.ChunkBytes);
        if (chunk.Total > LiveryCodec.MaxLiveryBytes || chunk.Count != count || chunk.Index >= chunk.Count
            || chunk.Data.Length != (chunk.Index + 1 < count ? LiveryCodec.ChunkBytes : (long)chunk.Total - (count - 1) * LiveryCodec.ChunkBytes)
            || !Register(from)) return [];
        if (_liveries.TryGetValue(from, out var stored))
        {
            if (stored.Sequence == chunk.Sequence) { QueueControl(from, new LiveryMessage.Stored(chunk.Sequence)); return Drain(); }
            if (!SequenceAfter(chunk.Sequence, stored.Sequence)) return [];
        }
        var fresh = true;
        if (_partials.TryGetValue(from, out var partial))
        {
            if (partial.Sequence == chunk.Sequence) fresh = false;
            else if (!SequenceAfter(chunk.Sequence, partial.Sequence)) return [];
        }
        if (fresh)
        {
            if (_newSequenceAt.TryGetValue(from, out var at) && _now - at < PutEveryMs)
            { Note(new LiveryNote.Refused(from, "more than one new livery per second")); return []; }
            _newSequenceAt[from] = _now; partial = new(chunk, _now); _partials[from] = partial;
        }
        if (partial is null) return [];
        if (partial.Total != chunk.Total || partial.Meta != chunk.Meta || partial.Parts.Length != chunk.Count)
        { _partials.Remove(from); Note(new LiveryNote.Refused(from, "chunks that disagree")); return []; }
        if (partial.Parts[chunk.Index] is null) { partial.Parts[chunk.Index] = chunk.Data.ToArray(); partial.Got++; }
        if (partial.Got < partial.Parts.Length) return [];
        _partials.Remove(from);
        var bytes = new byte[partial.Total]; var offset = 0;
        foreach (var part in partial.Parts) { part!.CopyTo(bytes, offset); offset += part.Length; }
        _liveries[from] = new(partial.Sequence, partial.Meta, bytes);
        Note(new LiveryNote.Stored(from, bytes.Length));
        QueueControl(from, new LiveryMessage.Stored(partial.Sequence));
        foreach (var (to, viewer) in _viewers)
            if (to != from) StartTransfer(to, viewer, from, partial.Sequence);
        return Drain();
    }

    internal List<LiveryOutput> OnWant(ulong from, long now)
    {
        Advance(now);
        if (!Allowed(from) || !Register(from)) return [];
        if (!_viewers.TryGetValue(from, out var viewer)) _viewers[from] = viewer = new();
        if (viewer.WantAt is { } at && _now - at < WantEveryMs) return [];
        viewer.WantAt = _now;
        // A hook repeats WANT while receiving a large design. Preserve its cursor, retry count,
        // original wait set and already acknowledged owners instead of restarting every second.
        if (viewer.SyncWait is null)
        {
            viewer.Generation++;
            viewer.SyncWait = _liveries.Keys.Where(owner => owner != from).ToHashSet();
            viewer.SyncCount = (byte)viewer.SyncWait.Count;
            foreach (var owner in viewer.SyncWait)
                if (!viewer.Pending.ContainsKey(owner)) StartTransfer(from, viewer, owner, _liveries[owner].Sequence);
        }
        else
            foreach (var pending in viewer.Pending.Values)
                if (!pending.Sending && pending.Tries < ResendMax) { pending.Sending = true; pending.NextChunk = 0; }
        ScheduleDue(); CheckDone(from, viewer); return Drain();
    }

    internal List<LiveryOutput> OnAck(ulong from, ulong owner, ushort sequence)
    {
        Advance(_now);
        if (!_viewers.TryGetValue(from, out var viewer)) return [];
        if (viewer.Pending.TryGetValue(owner, out var pending) && pending.Sequence == sequence)
        { RemoveTransfer(from, viewer, owner); viewer.SyncWait?.Remove(owner); }
        CheckDone(from, viewer); return Drain();
    }

    private void CheckDone(ulong to, Viewer viewer)
    {
        if (viewer.SyncWait is { Count: 0 }) QueueControl(to, new LiveryMessage.Done(viewer.SyncCount), viewer.Generation);
    }

    private void RemovePeer(ulong xuid)
    {
        var had = _liveries.Remove(xuid);
        _participants.Remove(xuid); _partials.Remove(xuid); _newSequenceAt.Remove(xuid);
        if (_viewers.Remove(xuid, out var departed))
            foreach (var owner in departed.Pending.Keys.ToArray()) RemoveTransfer(xuid, departed, owner);
        foreach (var (to, viewer) in _viewers)
        {
            RemoveTransfer(to, viewer, xuid); viewer.SyncWait?.Remove(xuid);
            if (had) QueueControl(to, new LiveryMessage.Gone(xuid));
            CheckDone(to, viewer);
        }
        // Remove commands destined for a departed identity so a later rejoin cannot receive them.
        var controls = _controls.Count;
        for (var i = 0; i < controls; i++)
        { var control = _controls.Dequeue(); if (control.Output.To != xuid) _controls.Enqueue(control); }
    }

    internal List<LiveryOutput> OnLeave(ulong xuid) { RemovePeer(xuid); return Drain(); }

    private void ScheduleDue()
    {
        foreach (var (to, viewer) in _viewers)
        {
            foreach (var (owner, pending) in viewer.Pending.ToArray())
            {
                if (_now - pending.Created >= TransferTtlMs || !pending.Sending && _now - pending.Sent >= ResendEveryMs && pending.Tries >= ResendMax)
                {
                    RemoveTransfer(to, viewer, owner); viewer.SyncWait?.Remove(owner);
                    Note(new LiveryNote.Skipped(owner, to)); continue;
                }
                if (!pending.Sending && _now - pending.Sent >= ResendEveryMs)
                { pending.Sending = true; pending.NextChunk = 0; }
            }
            CheckDone(to, viewer);
        }
    }

    internal List<LiveryOutput> Tick(long now)
    {
        Advance(now);
        foreach (var owner in _partials.Where(pair => _now - pair.Value.Started >= PartialTtlMs).Select(pair => pair.Key).ToArray())
            _partials.Remove(owner);
        ScheduleDue(); return Drain();
    }

    private static int WireBytes(LiveryMessage message) => message switch
    {
        LiveryMessage.Data data => 24 + ((34 + data.Chunk.Data.Length + 3) / 4) * 4,
        LiveryMessage.Gone => 36,
        _ => 28,
    };

    private bool TryOutput(List<LiveryOutput> output, LiveryOutput item)
    {
        var bytes = WireBytes(item.Message);
        if (WindowDatagrams >= MaxOutputDatagrams || WindowWireBytes + bytes > MaxOutputWireBytes) return false;
        output.Add(item); WindowDatagrams++; WindowWireBytes += bytes; return true;
    }

    private bool ControlCurrent(Control control)
    {
        if (!Allowed(control.Output.To)) return false;
        return control.Output.Message switch
        {
            LiveryMessage.Stored stored => _liveries.TryGetValue(control.Output.To, out var livery) && livery.Sequence == stored.Sequence,
            LiveryMessage.Gone gone => _viewers.ContainsKey(control.Output.To) && !_liveries.ContainsKey(gone.Owner),
            LiveryMessage.Done => _viewers.TryGetValue(control.Output.To, out var viewer) && viewer.Generation == control.Generation && viewer.SyncWait is { Count: 0 },
            _ => false,
        };
    }

    private List<LiveryOutput> Drain()
    {
        var output = new List<LiveryOutput>();
        while (_controls.TryPeek(out var control) && _windowControls < MaxControlDatagrams)
        {
            if (!ControlCurrent(control)) { _controls.Dequeue(); continue; }
            if (!TryOutput(output, control.Output)) break;
            _controls.Dequeue(); _windowControls++;
            if (control.Output.Message is LiveryMessage.Done) _viewers[control.Output.To].SyncWait = null;
        }
        var skipped = 0;
        while (_flows.Count != 0 && skipped < _flows.Count && WindowDatagrams < MaxOutputDatagrams)
        {
            var flow = _flows[_nextFlow];
            var viewer = _viewers[flow.To];
            var pending = viewer.Pending[flow.Owner];
            if (_now - pending.Created >= TransferTtlMs)
            {
                RemoveTransfer(flow.To, viewer, flow.Owner); viewer.SyncWait?.Remove(flow.Owner);
                Note(new LiveryNote.Skipped(flow.Owner, flow.To)); CheckDone(flow.To, viewer); continue;
            }
            if (!pending.Sending || !Allowed(flow.To) || !Allowed(flow.Owner))
            { _nextFlow = (_nextFlow + 1) % _flows.Count; skipped++; continue; }
            var livery = _liveries[flow.Owner];
            var start = pending.NextChunk * LiveryCodec.ChunkBytes;
            var length = Math.Min(LiveryCodec.ChunkBytes, livery.Bytes.Length - start);
            var wireBytes = 24 + ((34 + length + 3) / 4) * 4;
            if (WindowWireBytes + wireBytes > MaxOutputWireBytes) break;
            _nextFlow = (_nextFlow + 1) % _flows.Count;
            // Generate only the selected chunk: no full-livery Chunks() array or fanout copy.
            var count = Math.Max(1, (livery.Bytes.Length + LiveryCodec.ChunkBytes - 1) / LiveryCodec.ChunkBytes);
            var chunk = new LiveryChunk(livery.Sequence, (ushort)pending.NextChunk, (ushort)count,
                (uint)livery.Bytes.Length, livery.Meta, flow.Owner, livery.Bytes.AsSpan(start, length).ToArray());
            TryOutput(output, new(flow.To, new LiveryMessage.Data(chunk)));
            if (pending.NextChunk++ == 0) pending.Tries++;
            if (pending.NextChunk == count) { pending.Sending = false; pending.Sent = _now; }
            skipped = 0;
        }
        return output;
    }

    internal void Clear()
    {
        _participants.Clear(); _liveries.Clear(); _partials.Clear(); _newSequenceAt.Clear(); _viewers.Clear();
        _flows.Clear(); _controls.Clear(); _notes.Clear(); _nextFlow = 0; _now = 0; _windowAt = long.MinValue;
        WindowDatagrams = 0; WindowWireBytes = 0; _windowControls = 0;
    }
}

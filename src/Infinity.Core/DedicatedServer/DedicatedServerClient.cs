// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Buffers;
using System.Collections.Concurrent;
using System.Globalization;
using System.Text.Json;
using System.Text.Json.Serialization.Metadata;
using System.Threading.Channels;

namespace Infinity.Core.DedicatedServer;

/// <summary>A reply: the result, the engine's error, or (latest-wins commands only) superseded by a newer value.</summary>
public sealed record DsResult<T>
{
    public T? Value { get; init; }

    public DsError? Error { get; init; }

    /// <summary>A latest-wins command answered <c>busy</c> after a newer value of it was written: nothing was applied, the newer one stands.</summary>
    public bool Superseded { get; init; }

    public bool Ok => Error is null && !Superseded;
}

/// <summary>How a command is retried when the engine answers <c>busy</c> (§4.6: nothing was applied).</summary>
public enum DsBusyPolicy
{
    /// <summary>Returned to the caller (<c>hello</c>: a second controller).</summary>
    None,

    /// <summary>The same request again (<c>world.kick</c>, <c>world.close</c>, <c>world.gameSession.answer</c>, every other command).</summary>
    Exact,

    /// <summary>A snapshot: retried only while it is still the newest written value of its (op, world) (<c>world.context</c>, <c>world.admission</c>, <c>world.voice</c>, <c>world.voiceBlocks</c>, <c>world.watch</c>).</summary>
    LatestWins,
}

/// <summary>The channel is gone (EOF, a protocol violation, a stalled write, or disposed): every world it served is gone (§2.3).</summary>
public sealed class DedicatedServerGoneException(string reason) : IOException($"worlds channel closed: {reason}")
{
    public string Reason { get; } = reason;
}

public sealed record DsClientOptions
{
    /// <summary>One attempt's reply bound.</summary>
    public TimeSpan CommandTimeout { get; init; } = TimeSpan.FromSeconds(5);

    /// <summary>How long busy retries of one call may go on.</summary>
    public TimeSpan BusyRetryWindow { get; init; } = TimeSpan.FromSeconds(10);

    /// <summary>The longest single wait between busy retries (the engine's <c>retryAfterMs</c> is honoured up to this).</summary>
    public TimeSpan BusyRetryCap { get; init; } = TimeSpan.FromSeconds(1);

    /// <summary>The longest one write may wait for the engine to drain the pipe; past it the channel is closed as stalled.</summary>
    public TimeSpan WriteBound { get; init; } = TimeSpan.FromSeconds(2);

    /// <summary>Events held for the consumer; past it events are dropped and counted (<see cref="DedicatedServerClient.LocalEventsDropped"/>).</summary>
    public int EventCapacity { get; init; } = 4096;

    /// <summary>Log lines: op names, ids, codes and counts only, never XUIDs, addresses or keys. A throwing sink is ignored.</summary>
    public Action<string>? Log { get; init; }

    public TimeProvider Clock { get; init; } = TimeProvider.System;

    /// <summary>Completes <see cref="DedicatedServerClient.Events"/> when the channel closes; the process supervisor turns it off to publish <see cref="DsGone"/> first.</summary>
    public bool CompleteEventsOnClose { get; init; } = true;
}

/// <summary>
/// The controller side of the control channel (DEDICATED_SERVER.md §3-§4, proto 1) over any duplex stream: JSON lines,
/// request ids (u32, wrapping, never two in flight), replies correlated by id with a per-attempt timeout, the busy
/// rules of §4.6, events with their <c>seq</c> and <c>events.dropped</c>, and the §3.2 bound of 1 MiB a line. The
/// reader runs on a thread of its own (never the UI dispatcher) and no exception leaves it: a line that is not a JSON
/// object, or longer than the bound, is a protocol violation (the channel closes and <see cref="Violation"/> says
/// why; the process supervisor kills the child, §2.3); an event that does not match its shape is counted as malformed
/// and dropped. A write that times out or is cancelled may have left half a line, so it closes the channel as
/// <c>stalled</c>, a violation too.
/// </summary>
public sealed class DedicatedServerClient : IAsyncDisposable
{
    private const int MaxDropKinds = 32;
    private const int MaxKindLength = 64;

    private readonly Stream _stream;
    private readonly DsClientOptions _options;
    private readonly SemaphoreSlim _writes = new(1, 1);
    private readonly ConcurrentDictionary<uint, TaskCompletionSource<JsonElement>> _pending = new();
    private readonly ConcurrentDictionary<(string Op, string? World), long> _latest = new();
    private readonly ConcurrentDictionary<string, byte> _worlds = new(StringComparer.Ordinal);
    private readonly ConcurrentDictionary<string, long> _droppedKinds = new(StringComparer.Ordinal);
    private readonly Channel<DsEvent> _events;
    private readonly TaskCompletionSource<string> _closed = new(TaskCreationOptions.RunContinuationsAsynchronously);
    private readonly TaskCompletionSource _readerDone = new(TaskCreationOptions.RunContinuationsAsynchronously);
    private readonly CancellationTokenSource _lifetime = new();
    private Thread? _reader;
    private uint _nextId;
    private long _generation;
    private long _lastSeq;
    private long _seqGaps;
    private long _droppedTotal;
    private long _localDropped;
    private long _unknownEvents;
    private long _malformedEvents;
    private long _lateReplies;
    private int _closing;
    private int _violated;

    public DedicatedServerClient(Stream stream, DsClientOptions? options = null)
    {
        _stream = stream ?? throw new ArgumentNullException(nameof(stream));
        _options = options ?? new DsClientOptions();
        // one slot is always kept for the supervisor's final DsGone
        _events = Channel.CreateBounded<DsEvent>(new BoundedChannelOptions(Math.Max(2, _options.EventCapacity)) { SingleReader = true, FullMode = BoundedChannelFullMode.Wait });
    }

    /// <summary>Events in arrival order; after the channel closes, completed (or <see cref="DsGone"/> then completed, by the supervisor).</summary>
    public ChannelReader<DsEvent> Events => _events.Reader;

    /// <summary>Completes with the reason when the channel is gone.</summary>
    public Task<string> Closed => _closed.Task;

    /// <summary>Why the channel was closed for a protocol violation (<c>json</c>, <c>line</c>, <c>stalled</c>); null otherwise.</summary>
    public string? Violation { get; private set; }

    /// <summary>Raised once for a protocol violation (before <see cref="Closed"/> completes); exceptions it throws are swallowed.</summary>
    public Action<string>? OnViolation { get; set; }

    /// <summary>The worlds open now (world.open ok, minus world.closed / world.close).</summary>
    public IReadOnlyCollection<string> OpenWorlds => _worlds.Keys.ToArray();

    public long LastSeq => Interlocked.Read(ref _lastSeq);

    /// <summary>
    /// Sequence numbers skipped. The engine assigns <c>seq</c> at dequeue, gapless, and reports drops only in
    /// <c>events.dropped</c> (§3.2), so a gap is an engine anomaly: logged and counted here, never a kill (the events
    /// that did arrive are still valid, and <c>world.state</c> is never dropped).
    /// </summary>
    public long SeqGaps => Interlocked.Read(ref _seqGaps);

    /// <summary>The sum of every non-negative <c>events.dropped.count</c> (the engine's drops).</summary>
    public long EventsDropped => Interlocked.Read(ref _droppedTotal);

    /// <summary>The engine's drops by kind (at most 32 kinds, names scrubbed).</summary>
    public IReadOnlyDictionary<string, long> EventsDroppedByKind => new Dictionary<string, long>(_droppedKinds, StringComparer.Ordinal);

    /// <summary>Events this client dropped because its consumer fell <see cref="DsClientOptions.EventCapacity"/> behind.</summary>
    public long LocalEventsDropped => Interlocked.Read(ref _localDropped);

    public long UnknownEvents => Interlocked.Read(ref _unknownEvents);

    public long MalformedEvents => Interlocked.Read(ref _malformedEvents);

    public long LateReplies => Interlocked.Read(ref _lateReplies);

    public bool IsClosed => _closed.Task.IsCompleted;

    /// <summary>Starts the reader thread. Call once.</summary>
    public void Start()
    {
        if (_reader is not null)
        {
            throw new InvalidOperationException("already started");
        }
        _reader = new Thread(ReadLoop) { IsBackground = true, Name = "fi-ds-control" };
        _reader.Start();
    }

    public static DsBusyPolicy PolicyOf(string op) => op switch
    {
        DsOps.Hello => DsBusyPolicy.None,
        DsOps.WorldContext or DsOps.WorldAdmission or DsOps.WorldVoice or DsOps.WorldVoiceBlocks or DsOps.WorldWatch or DsOps.CatalogSet => DsBusyPolicy.LatestWins,
        _ => DsBusyPolicy.Exact,
    };

    // ---- typed commands ----

    public Task<DsResult<DsHelloResult>> HelloAsync(string? secret, string controllerVersion, CancellationToken cancellation = default) =>
        SendAsync(new DsHello
        {
            Proto = DsProto.Version,
            Secret = secret,
            Controller = new DsControllerInfo { Kind = "launcher", Version = controllerVersion },
        }, DsJson.Default.DsHelloResult, cancellation);

    public Task<DsResult<DsKeysSetResult>> SetKeysAsync(string ring, CancellationToken cancellation = default) =>
        SendAsync(new DsKeysSet { Ring = ring }, DsJson.Default.DsKeysSetResult, cancellation);

    /// <summary><c>catalog.set</c> (§9): the Road Trip rows; latest-wins.</summary>
    public Task<DsResult<DsCatalogSetResult>> SetCatalogAsync(DsCatalogSet catalog, CancellationToken cancellation = default) =>
        SendAsync(catalog, DsJson.Default.DsCatalogSetResult, cancellation);

    /// <summary><c>catalog.parts</c> (§9): the answer to a <see cref="DsCatalogNeed"/>.</summary>
    public Task<DsResult<DsEmpty>> SetCatalogPartsAsync(DsCatalogParts parts, CancellationToken cancellation = default) =>
        SendAsync(parts, DsJson.Default.DsEmpty, cancellation);

    public Task<DsResult<DsCapacity>> CapacityAsync(CancellationToken cancellation = default) =>
        SendAsync(new DsBare(DsOps.Capacity), DsJson.Default.DsCapacity, cancellation);

    public Task<DsResult<DsHealth>> HealthAsync(CancellationToken cancellation = default) =>
        SendAsync(new DsBare(DsOps.Health), DsJson.Default.DsHealth, cancellation);

    public Task<DsResult<JsonElement>> StatsAsync(CancellationToken cancellation = default) =>
        SendAsync(new DsBare(DsOps.Stats), DsJson.Default.JsonElement, cancellation);

    public Task<DsResult<DsEmpty>> DrainAsync(int? deadlineSec = null, CancellationToken cancellation = default) =>
        SendAsync(new DsDrain { DeadlineSec = deadlineSec }, DsJson.Default.DsEmpty, cancellation);

    public Task<DsResult<DsEmpty>> ShutdownAsync(CancellationToken cancellation = default) =>
        SendAsync(new DsBare(DsOps.Shutdown), DsJson.Default.DsEmpty, cancellation);

    public Task<DsResult<DsWorldOpenResult>> OpenWorldAsync(DsWorldOpen open, CancellationToken cancellation = default) =>
        SendAsync(open, DsJson.Default.DsWorldOpenResult, cancellation);

    public Task<DsResult<DsContextResult>> ContextAsync(DsWorldContext context, CancellationToken cancellation = default) =>
        SendAsync(context, DsJson.Default.DsContextResult, cancellation);

    public Task<DsResult<DsAdmissionResult>> AdmissionAsync(DsWorldAdmission admission, CancellationToken cancellation = default) =>
        SendAsync(admission, DsJson.Default.DsAdmissionResult, cancellation);

    public Task<DsResult<DsEmpty>> EndAdmissionAsync(DsWorldAdmissionEnd end, CancellationToken cancellation = default) =>
        SendAsync(end, DsJson.Default.DsEmpty, cancellation);

    public Task<DsResult<DsEmpty>> VoiceAsync(DsWorldVoice voice, CancellationToken cancellation = default) =>
        SendAsync(voice, DsJson.Default.DsEmpty, cancellation);

    public Task<DsResult<DsVoiceBlocksResult>> VoiceBlocksAsync(DsWorldVoiceBlocks blocks, CancellationToken cancellation = default) =>
        SendAsync(blocks, DsJson.Default.DsVoiceBlocksResult, cancellation);

    public Task<DsResult<DsEmpty>> WatchAsync(DsWorldWatch watch, CancellationToken cancellation = default) =>
        SendAsync(watch, DsJson.Default.DsEmpty, cancellation);

    public Task<DsResult<DsKickResult>> KickAsync(DsWorldKick kick, CancellationToken cancellation = default) =>
        SendAsync(kick, DsJson.Default.DsKickResult, cancellation);

    public Task<DsResult<DsEmpty>> CloseWorldAsync(DsWorldClose close, CancellationToken cancellation = default) =>
        SendAsync(close, DsJson.Default.DsEmpty, cancellation);

    public Task<DsResult<DsWorldState>> StatusAsync(string worldId, CancellationToken cancellation = default) =>
        SendAsync(new DsWorldStatus { WorldId = worldId }, DsJson.Default.DsWorldState, cancellation);

    public Task<DsResult<DsEmpty>> AnswerGameSessionAsync(DsGameSessionAnswer answer, CancellationToken cancellation = default) =>
        SendAsync(answer, DsJson.Default.DsEmpty, cancellation);

    /// <summary>
    /// Sends <paramref name="request"/> and awaits its reply, retrying <c>busy</c> by <see cref="PolicyOf"/>. Throws
    /// <see cref="TimeoutException"/> when one attempt has no reply within <see cref="DsClientOptions.CommandTimeout"/>
    /// (a late reply is then ignored) and <see cref="DedicatedServerGoneException"/> when the channel is gone. A reply
    /// that does not parse as its result is the typed error <see cref="DsErrorCodes.MalformedReply"/>.
    /// </summary>
    public async Task<DsResult<T>> SendAsync<T>(IDsRequest request, JsonTypeInfo<T> result, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(request);
        ArgumentNullException.ThrowIfNull(result);
        var policy = PolicyOf(request.Op);
        var key = (request.Op, request.TargetWorld());
        var clock = _options.Clock;
        var started = clock.GetTimestamp();
        var attempts = 0;
        long generation = 0;
        while (true)
        {
            // Latest-wins: the generation is taken, and the newest check made, under the write lock right before each
            // write, so generations follow the order of the writes and an older value is never written after a newer one.
            Func<bool>? gate = policy != DsBusyPolicy.LatestWins ? null : attempts == 0
                ? () =>
                {
                    generation = Interlocked.Increment(ref _generation);
                    _latest[key] = generation;
                    return true;
                }
                : () => _latest.TryGetValue(key, out var newest) && newest == generation;
            var reply = await RoundTripAsync(request, gate, cancellation).ConfigureAwait(false);
            if (reply is not { } element)
            {
                return new DsResult<T> { Superseded = true };
            }
            var error = ErrorOf(element);
            if (error is null)
            {
                T? value;
                try
                {
                    value = element.Deserialize(result);
                }
                catch (Exception e) when (e is JsonException or NotSupportedException or InvalidOperationException or ArgumentException)
                {
                    Log("ds malformed reply: " + request.Op);
                    return new DsResult<T> { Error = new DsError { Code = DsErrorCodes.MalformedReply, Reason = e.GetType().Name } };
                }
                Track(request, value);
                return new DsResult<T> { Value = value };
            }
            if (error.Code != DsErrorCodes.Busy || policy == DsBusyPolicy.None)
            {
                return new DsResult<T> { Error = error };
            }
            attempts++;
            var wait = TimeSpan.FromMilliseconds(Math.Clamp(error.RetryAfterMs ?? DsProto.DefaultBusyRetryMs, 20, (long)_options.BusyRetryCap.TotalMilliseconds));
            if (clock.GetElapsedTime(started) + wait > _options.BusyRetryWindow)
            {
                Log(FormattableString.Invariant($"ds busy: {request.Op} gave up after {attempts} attempts"));
                return new DsResult<T> { Error = error };
            }
            await Task.Delay(wait, clock, cancellation).ConfigureAwait(false);
        }
    }

    private void Track<T>(IDsRequest request, T value)
    {
        switch (request)
        {
            case DsWorldOpen open when value is DsWorldOpenResult:
                _worlds[open.WorldId] = 0;
                break;
            case DsWorldClose close:
                Forget(close.WorldId);
                break;
            case DsHello when value is DsHelloResult hello:
                foreach (var world in hello.Worlds ?? [])
                {
                    if (world is not null)
                    {
                        _worlds[world] = 0;
                    }
                }
                break;
        }
    }

    /// <summary>A world is gone: its open entry and its latest-wins slots go too.</summary>
    private void Forget(string? worldId)
    {
        if (worldId is null)
        {
            return;
        }
        _worlds.TryRemove(worldId, out _);
        foreach (var key in _latest.Keys.Where(k => string.Equals(k.World, worldId, StringComparison.Ordinal)).ToArray())
        {
            _latest.TryRemove(key, out _);
        }
    }

    private static DsError? ErrorOf(JsonElement reply)
    {
        if (reply.TryGetProperty("ok", out var ok) && ok.ValueKind == JsonValueKind.True)
        {
            return null;
        }
        if (reply.TryGetProperty("error", out var error) && error.ValueKind == JsonValueKind.Object)
        {
            try
            {
                return error.Deserialize(DsJson.Default.DsError) ?? new DsError { Code = DsErrorCodes.MalformedReply };
            }
            catch (Exception e) when (e is JsonException or NotSupportedException or InvalidOperationException)
            {
                return new DsError { Code = DsErrorCodes.MalformedReply, Reason = "error" };
            }
        }
        return new DsError { Code = DsErrorCodes.MalformedReply, Reason = "reply" };
    }

    /// <summary>One attempt; null when <paramref name="gate"/> said no under the write lock (superseded, nothing written).</summary>
    private async Task<JsonElement?> RoundTripAsync(IDsRequest request, Func<bool>? gate, CancellationToken cancellation)
    {
        if (_closed.Task.IsCompleted)
        {
            throw new DedicatedServerGoneException(_closed.Task.Result);
        }
        var (id, waiter) = Reserve();
        try
        {
            if (!await WriteAsync(Encode(id, request), gate, cancellation).ConfigureAwait(false))
            {
                return null;
            }
            using var bound = CancellationTokenSource.CreateLinkedTokenSource(cancellation, _lifetime.Token);
            try
            {
                return await waiter.Task.WaitAsync(_options.CommandTimeout, _options.Clock, bound.Token).ConfigureAwait(false);
            }
            catch (TimeoutException)
            {
                Log(FormattableString.Invariant($"ds timeout: {request.Op} id={id}"));
                throw new TimeoutException($"worlds did not answer {request.Op} in time");
            }
            catch (OperationCanceledException) when (!cancellation.IsCancellationRequested)
            {
                throw new DedicatedServerGoneException(_closed.Task.IsCompleted ? _closed.Task.Result : "closed");
            }
        }
        finally
        {
            _pending.TryRemove(id, out _);
        }
    }

    private (uint Id, TaskCompletionSource<JsonElement> Waiter) Reserve()
    {
        var waiter = new TaskCompletionSource<JsonElement>(TaskCreationOptions.RunContinuationsAsynchronously);
        while (true)
        {
            // u32 ids, wrapping (§3.2: unique among the requests in flight)
            var id = Interlocked.Increment(ref _nextId);
            if (_pending.TryAdd(id, waiter))
            {
                return (id, waiter);
            }
        }
    }

    /// <summary>One request line: <c>{"id":n,"op":"…",…fields}\n</c>.</summary>
    internal static byte[] Encode(uint id, IDsRequest request)
    {
        var body = JsonSerializer.SerializeToElement(request, request.GetType(), DsJson.Default);
        var buffer = new ArrayBufferWriter<byte>(256);
        using (var writer = new Utf8JsonWriter(buffer))
        {
            writer.WriteStartObject();
            writer.WriteNumber("id", id);
            writer.WriteString("op", request.Op);
            foreach (var property in body.EnumerateObject())
            {
                if (property.NameEquals("id") || property.NameEquals("op"))
                {
                    continue;
                }
                property.WriteTo(writer);
            }
            writer.WriteEndObject();
        }
        if (buffer.WrittenCount >= DsProto.MaxLine)
        {
            throw new ArgumentException($"{request.Op} is over the 1 MiB line bound", nameof(request));
        }
        buffer.Write("\n"u8);
        return buffer.WrittenSpan.ToArray();
    }

    /// <summary>
    /// Writes one line under the write lock; false when <paramref name="gate"/> refused (nothing written). A write that
    /// started and then timed out or was cancelled may have left half a line: the channel closes as <c>stalled</c>.
    /// </summary>
    private async Task<bool> WriteAsync(byte[] line, Func<bool>? gate, CancellationToken cancellation)
    {
        // The caller's token covers only the wait for the lock. Once the lock is held the write runs under WriteBound
        // and the channel's lifetime alone, so a caller cancelling never cuts a line in half (and never kills the child).
        using (var waiting = CancellationTokenSource.CreateLinkedTokenSource(cancellation, _lifetime.Token))
        {
            try
            {
                await _writes.WaitAsync(waiting.Token).ConfigureAwait(false);
            }
            catch (OperationCanceledException) when (!cancellation.IsCancellationRequested)
            {
                throw new DedicatedServerGoneException(_closed.Task.IsCompleted ? _closed.Task.Result : "closed");
            }
            catch (ObjectDisposedException)
            {
                throw new DedicatedServerGoneException("disposed");
            }
        }
        try
        {
            if (_closed.Task.IsCompleted)
            {
                throw new DedicatedServerGoneException(_closed.Task.Result);
            }
            if (gate is not null && !gate())
            {
                return false;
            }
            using var bound = CancellationTokenSource.CreateLinkedTokenSource(_lifetime.Token);
            bound.CancelAfter(_options.WriteBound);
            try
            {
                await _stream.WriteAsync(line, bound.Token).ConfigureAwait(false);
                await _stream.FlushAsync(bound.Token).ConfigureAwait(false);
            }
            catch (OperationCanceledException)
            {
                if (_lifetime.IsCancellationRequested)
                {
                    throw new DedicatedServerGoneException(_closed.Task.IsCompleted ? _closed.Task.Result : "closed");
                }
                // past WriteBound: half a line may be in the pipe, the channel cannot be trusted any more (§4.6)
                Violate("stalled");
                throw new DedicatedServerGoneException("violation: stalled");
            }
            return true;
        }
        catch (Exception e) when (e is IOException and not DedicatedServerGoneException or ObjectDisposedException or InvalidOperationException)
        {
            Close("write failed");
            throw new DedicatedServerGoneException("write failed: " + e.GetType().Name);
        }
        finally
        {
            _writes.Release();
        }
    }
    // ---- the reader ----

    private void ReadLoop()
    {
        var reason = "eof";
        try
        {
            var framer = new DsLineFramer();
            var chunk = new byte[64 * 1024];
            while (!_lifetime.IsCancellationRequested && Violation is null)
            {
                var read = _stream.Read(chunk, 0, chunk.Length);
                if (read == 0)
                {
                    break;
                }
                if (!framer.Push(chunk.AsSpan(0, read), Line))
                {
                    Violate("line");
                }
            }
        }
        catch (Exception e) when (e is IOException or ObjectDisposedException or OperationCanceledException or InvalidOperationException)
        {
            reason = e is IOException ? "broken" : "disposed";
        }
#pragma warning disable CA1031 // the reader thread must never die with an exception (it would end the launcher)
        catch (Exception e)
#pragma warning restore CA1031
        {
            // never a silent end: an unexpected reader fault is a violation (the supervisor kills the child)
            reason = "fault: " + e.GetType().Name;
            Log("ds reader fault: " + e.GetType().Name);
            Violate("reader");
        }
        finally
        {
            Close(reason);
            _readerDone.TrySetResult();
        }
    }

    private void Line(ReadOnlyMemory<byte> line)
    {
        if (Violation is not null)
        {
            return;
        }
        JsonDocument document;
        try
        {
            document = JsonDocument.Parse(line, new JsonDocumentOptions { MaxDepth = 64 });
        }
        catch (JsonException)
        {
            Violate("json");
            return;
        }
        using (document)
        {
            var root = document.RootElement;
            if (root.ValueKind != JsonValueKind.Object)
            {
                Violate("json");
                return;
            }
#pragma warning disable CA1031 // one bad line never ends the reader: it is counted and dropped
            try
            {
                if (root.TryGetProperty("ev", out var ev) && ev.ValueKind == JsonValueKind.String)
                {
                    OnEvent(ev.GetString()!, root);
                    return;
                }
                OnReply(root);
            }
            catch (Exception e)
            {
                Interlocked.Increment(ref _malformedEvents);
                Log("ds line dropped: " + e.GetType().Name);
            }
#pragma warning restore CA1031
        }
    }

    private void OnReply(JsonElement root)
    {
        if (!root.TryGetProperty("id", out var idElement))
        {
            Log("ds line with neither id nor ev ignored");
            return;
        }
        if (idElement.ValueKind == JsonValueKind.Null)
        {
            // the engine refused one of our lines before reading its id (§3.2): our bug, never theirs
            Log("ds framing error reply: " + (ErrorOf(root)?.ToString() ?? "?"));
            return;
        }
        if (idElement.ValueKind != JsonValueKind.Number || !idElement.TryGetUInt32(out var id))
        {
            Log("ds reply with an invalid id ignored");
            return;
        }
        if (_pending.TryRemove(id, out var waiter))
        {
            waiter.TrySetResult(root.Clone());
        }
        else
        {
            Interlocked.Increment(ref _lateReplies);
            Log(FormattableString.Invariant($"ds late reply id={id} ignored"));
        }
    }

    private void OnEvent(string ev, JsonElement root)
    {
        long seq = 0;
        if (root.TryGetProperty("seq", out var seqElement) && seqElement.ValueKind == JsonValueKind.Number && seqElement.TryGetInt64(out seq))
        {
            var last = Interlocked.Exchange(ref _lastSeq, seq);
            if (last != 0 && seq != last + 1)
            {
                Interlocked.Add(ref _seqGaps, Math.Max(0, seq - last - 1));
                Log(FormattableString.Invariant($"ds anomaly: event seq gap {last} -> {seq}"));
            }
        }
        DsEvent? parsed;
        try
        {
            parsed = ev switch
            {
                DsEventNames.WorldState => root.Deserialize(DsJson.Default.DsWorldState),
                DsEventNames.PeerSynced => root.Deserialize(DsJson.Default.DsPeerSynced),
                DsEventNames.PeerLeft => root.Deserialize(DsJson.Default.DsPeerLeft),
                DsEventNames.WorldEmpty => root.Deserialize(DsJson.Default.DsWorldEmpty),
                DsEventNames.WorldOccupied => root.Deserialize(DsJson.Default.DsWorldOccupied),
                DsEventNames.WorldClosed => root.Deserialize(DsJson.Default.DsWorldClosed),
                DsEventNames.WorldNetwork => root.Deserialize(DsJson.Default.DsNetworkFacts) is { } facts ? new DsWorldNetwork { Facts = facts, Seq = seq } : null,
                DsEventNames.GameSessionRequest => root.Deserialize(DsJson.Default.DsGameSessionRequest),
                DsEventNames.WatchReport => root.Deserialize(DsJson.Default.DsWatchReport),
                DsEventNames.PeerRejected => root.Deserialize(DsJson.Default.DsPeerRejected),
                DsEventNames.Diag => new DsDiag
                {
                    Seq = seq,
                    WorldId = root.TryGetProperty("worldId", out var w) && w.ValueKind == JsonValueKind.String ? w.GetString() : null,
                    Body = root.Clone(),
                },
                DsEventNames.Notice => root.Deserialize(DsJson.Default.DsNotice),
                DsEventNames.Draining => root.Deserialize(DsJson.Default.DsDraining),
                DsEventNames.EventsDropped => root.Deserialize(DsJson.Default.DsEventsDropped),
                DsEventNames.CatalogNeed => root.Deserialize(DsJson.Default.DsCatalogNeed),
                _ => Unknown(),
            };
        }
        catch (Exception e) when (e is JsonException or NotSupportedException or InvalidOperationException or ArgumentException)
        {
            Malformed(ev, e.GetType().Name);
            return;
        }
        if (parsed is null)
        {
            return;
        }
        if (!Valid(parsed))
        {
            Malformed(ev, "fields");
            return;
        }
        switch (parsed)
        {
            case DsWorldClosed closed:
                Forget(closed.WorldId);
                break;
            case DsEventsDropped dropped:
                parsed = dropped with { Kinds = CountDrops(dropped) };
                break;
        }
        Publish(parsed);
    }

    private DsEvent? Unknown()
    {
        Interlocked.Increment(ref _unknownEvents);
        return null;
    }

    private void Malformed(string ev, string why)
    {
        Interlocked.Increment(ref _malformedEvents);
        Log("ds malformed event ignored: " + Kind(ev) + " (" + why + ")");
    }

    /// <summary>The fields every event's consumers rely on (a JSON null in a required field never reaches them).</summary>
    private static bool Valid(DsEvent e) => e switch
    {
        DsWorldState s => s.WorldId is not null && s.Peers is not null && s.Peers.All(p => p is not null) && s.Engines is not null,
        DsPeerSynced s => s.WorldId is not null && s.Xuid is not null,
        DsPeerLeft s => s.WorldId is not null && s.Xuid is not null,
        DsWorldEmpty s => s.WorldId is not null,
        DsWorldOccupied s => s.WorldId is not null,
        DsWorldClosed s => s.WorldId is not null && s.Reason is not null,
        DsWorldNetwork s => s.Facts.PublicAddresses is not null,
        DsGameSessionRequest s => s.WorldId is not null && s.Kind is not null && s.Xuids is not null && s.Xuids.All(x => x is not null),
        DsWatchReport s => s.WorldId is not null && s.Id is not null && s.Xuid is not null && s.Kind is not null,
        DsPeerRejected s => s.WorldId is not null && s.Kind is not null && s.Valid,
        DsNotice s => s.Id is not null,
        DsEventsDropped s => s.Kinds is not null,
        DsCatalogNeed s => s.Game == DsProto.GameFh2 && s.Kind == "parts",
        _ => true,
    };

    /// <summary>Counts the engine's drops: negative counts ignored, at most 32 kinds, names bounded and scrubbed.</summary>
    private Dictionary<string, long> CountDrops(DsEventsDropped dropped)
    {
        if (dropped.Count > 0)
        {
            Interlocked.Add(ref _droppedTotal, dropped.Count);
        }
        var kinds = new Dictionary<string, long>(StringComparer.Ordinal);
        foreach (var (raw, count) in dropped.Kinds)
        {
            if (count <= 0 || raw is null)
            {
                continue;
            }
            var kind = Kind(raw);
            if (!_droppedKinds.ContainsKey(kind) && _droppedKinds.Count >= MaxDropKinds)
            {
                kind = "other";
            }
            _droppedKinds.AddOrUpdate(kind, count, (_, old) => old + count);
            kinds[kind] = kinds.GetValueOrDefault(kind) + count;
        }
        Log(string.Create(CultureInfo.InvariantCulture, $"ds events dropped: {Math.Max(0, dropped.Count)} ({string.Join(',', kinds.Select(k => $"{k.Key}={k.Value}"))})"));
        return kinds;
    }

    /// <summary>An event-kind name fit for a log: <c>[A-Za-z0-9._]</c>, at most 64 characters.</summary>
    private static string Kind(string raw)
    {
        var clean = new string(raw.Take(MaxKindLength).Select(c => char.IsAsciiLetterOrDigit(c) || c is '.' or '_' ? c : '?').ToArray());
        return clean.Length == 0 ? "?" : clean;
    }

    private void Publish(DsEvent e)
    {
        // the last slot is the supervisor's (DsGone)
        if (_events.Reader.Count >= Math.Max(2, _options.EventCapacity) - 1 || !_events.Writer.TryWrite(e))
        {
            if (Interlocked.Increment(ref _localDropped) == 1)
            {
                Log("ds events: the consumer fell behind; dropping");
            }
        }
    }

    private void Violate(string field)
    {
        if (Interlocked.Exchange(ref _violated, 1) != 0)
        {
            return;
        }
        Violation = field;
        Log("ds protocol violation: " + field);
#pragma warning disable CA1031 // the handler (the supervisor's kill) never takes the reader or a writer down with it
        try
        {
            OnViolation?.Invoke(field);
        }
        catch (Exception e)
        {
            Log("ds violation handler failed: " + e.GetType().Name);
        }
#pragma warning restore CA1031
        Close("violation: " + field);
    }

    private void Close(string reason)
    {
        if (Interlocked.Exchange(ref _closing, 1) != 0)
        {
            return;
        }
        _closed.TrySetResult(reason);
        foreach (var (id, waiter) in _pending)
        {
            if (_pending.TryRemove(id, out _))
            {
                waiter.TrySetException(new DedicatedServerGoneException(reason));
            }
        }
        if (_options.CompleteEventsOnClose)
        {
            _events.Writer.TryComplete();
        }
    }

    /// <summary>For the supervisor: one last event (<see cref="DsGone"/>) after the channel closed, then the end.</summary>
    internal void Finish(DsEvent? last)
    {
        if (last is not null)
        {
            _events.Writer.TryWrite(last);
        }
        _events.Writer.TryComplete();
    }

    private void Log(string line)
    {
#pragma warning disable CA1031 // a log sink never breaks the channel
        try
        {
            _options.Log?.Invoke(line);
        }
        catch (Exception)
        {
            // ignored
        }
#pragma warning restore CA1031
    }

    /// <summary>Closes the channel and waits (bounded, asynchronously; never a thread join) for the reader to end.</summary>
    public async ValueTask DisposeAsync()
    {
        Close("disposed");
        await _lifetime.CancelAsync().ConfigureAwait(false);
        try
        {
            await _stream.DisposeAsync().ConfigureAwait(false);
        }
        catch (Exception e) when (e is IOException or ObjectDisposedException)
        {
            // already broken
        }
        if (_reader is not null)
        {
            await Task.WhenAny(_readerDone.Task, Task.Delay(TimeSpan.FromSeconds(2))).ConfigureAwait(false);
        }
    }
}

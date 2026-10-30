// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Diagnostics;
using System.Globalization;
using System.Net;
using System.Net.Sockets;
using System.Threading.Channels;
using Infinity.Core.WorldServer;

namespace Infinity.Core.Matchmaking;

/// <summary>Launcher-owned UDP world. Every engine operation runs on one bounded background actor.</summary>
public sealed class IntegratedHostRelay : IHostRelay
{
    private sealed record Command(Action Work, TaskCompletionSource? Completion = null, CancellationToken Cancellation = default);
    private readonly Socket _socket;
    private readonly RelaySettings _settings;
    private readonly RelayEngine _engine; // free roam (every lobby type but a car meet)
    private readonly Dictionary<uint, RelayEngine> _carMeets = []; // actor thread only, at most the two car meet lobby types
    private readonly Dictionary<IPEndPoint, RelayEngine> _routes = []; // actor thread only: endpoints seated in a car meet
    private Func<ulong, IPEndPoint, long, bool> _authorize = static (_, _, _) => false;
    private Func<ulong, IPEndPoint, long, bool>? _authorizeHook;
    private readonly RelayNetworkNat _nat;
    private readonly RelayLegs _legs; // CR-S57: the joiners met at a relay (actor thread only)
    private readonly CancellationTokenSource _stop = new();
    private readonly Channel<Command> _commands = Channel.CreateBounded<Command>(new BoundedChannelOptions(256)
    { SingleReader = true, FullMode = BoundedChannelFullMode.Wait });
    private readonly Stopwatch _clock = Stopwatch.StartNew();
    private readonly Task _actor;
    private readonly Task _discovery;
    private readonly Task _upnp;
    private IReadOnlyList<string> _peers = Array.Empty<string>();
    private RelayNetworkStatus _network;
    private int _disposed, _running = 1;
    private uint _sendErrors; // actor thread only
    private Diagnostics.CrashContext? _crashContext;
    private Func<ReadOnlyMemory<byte>,IPEndPoint,byte[]?>? _proofReceiver;

    public Diagnostics.CrashContext? CrashContext { get => Volatile.Read(ref _crashContext); set => Volatile.Write(ref _crashContext, value); }
    private Action<string>? _diagnosticLog;
    /// <summary>The launcher log the world's connection lines go to (written off the actor thread).</summary>
    public Action<string>? DiagnosticLog { get => Volatile.Read(ref _diagnosticLog); set => Volatile.Write(ref _diagnosticLog, value); }

    private IntegratedHostRelay(Socket socket, uint maxPlayers, bool races, IntegratedRelayOptions options)
    {
        _socket = socket;
        Port = checked((ushort)((IPEndPoint)socket.LocalEndPoint!).Port);
        var dual = socket.AddressFamily == AddressFamily.InterNetworkV6;
        _nat = new(dual, (long)options.StunInterval.TotalMilliseconds, Send);
        _legs = new(SendRaw);
        if (options.EnableDiscovery && options.EnableUpnp) _nat.SetUpnp(RelayUpnpState.Pending);
        _network = _nat.Snapshot();
        _settings = new RelaySettings { MaxPlayers = checked((int)Math.Clamp(maxPlayers, 1u, 12u)), Races = races };
        _engine = NewEngine(null);
        var cancellation = _stop.Token;
        _actor = Task.Factory.StartNew(() => Run(cancellation), CancellationToken.None, TaskCreationOptions.LongRunning, TaskScheduler.Default);
        var serverNames = options.StunServers.Take(8).ToArray();
        _discovery = options.EnableDiscovery ? Task.Run(() => DiscoverAsync(serverNames, cancellation), cancellation) : Task.CompletedTask;
        _upnp = options.EnableDiscovery && options.EnableUpnp
            ? Task.Run(() => RelayNetworkUpnp.RunAsync(Port, mapping => Enqueue(() => _nat.SetUpnp(mapping.State, mapping.Address)), cancellation), cancellation) : Task.CompletedTask;
    }

    private RelayEngine NewEngine(uint? carMeet)
    {
        var world = carMeet is null ? "free-roam engine" : "car meet engine";
        // World admission must be installed before any game datagram can establish a seat.
        return new(_settings with { CarMeetLobby = carMeet }, Send)
        {
            Authorize = _authorize, AuthorizeHook = _authorizeHook,
            ValidationRejected = (sender, kind, message, router, value) => Volatile.Read(ref _crashContext)?.Rejected(sender, kind, message, router, value),
            ConnectionOpened = (index, a, b, refusal) =>
            {
                // A is the lobby type the game asks for (1 Road Trip, 2 free roam, 3 car meet); no identity or address is logged
                if (Volatile.Read(ref _diagnosticLog) is not { } sink) return;
                var line = refusal is { } reason
                    ? string.Create(CultureInfo.InvariantCulture, $"world: OpenConnection refused reason {reason} lobby type {a} at {world} (A={a} B={b})")
                    : string.Create(CultureInfo.InvariantCulture, $"world: OpenConnection admitted peer {index} lobby type {a} -> {world} (A={a} B={b})");
                Diagnostics.UdpDiagnostics.Enqueue(sink, line);
            },
            WorldLog = line =>
            {
                if (Volatile.Read(ref _diagnosticLog) is { } sink) Diagnostics.UdpDiagnostics.Enqueue(sink, $"world: {world}: {line}");
            },
        };
    }

    private IEnumerable<RelayEngine> Engines => _carMeets.Values.Prepend(_engine);

    /// <summary>
    /// One host socket serves one world per lobby kind: free roam (and every other A) in <see cref="_engine"/>, each car meet
    /// lobby type in its own. An OpenConnection goes to its lobby's world; once admitted there, its endpoint's later datagrams
    /// follow it and its seat in the world it left is dropped (the game left that session). Guests who stayed keep theirs.
    /// </summary>
    private void Route(ReadOnlyMemory<byte> datagram, IPEndPoint peer, long now)
    {
        if (StunCodec.IsStun(datagram.Span))
        {
            // hook side channels name their player: the world holding the endpoint, else every world (only the one seating it acts)
            if (_routes.TryGetValue(peer, out var holder)) holder.Receive(datagram.Span, peer, now);
            else foreach (var engine in Engines.ToArray()) engine.Receive(datagram.Span, peer, now);
            return;
        }
        var current = _routes.GetValueOrDefault(peer) ?? _engine;
        if (RelayEngine.LobbyTypeOf(datagram.Span) is not { } lobby) { current.Receive(datagram.Span, peer, now); return; }
        var target = RelayEngine.IsCarMeetLobby(lobby) ? CarMeet(lobby) : _engine;
        target.Receive(datagram.Span, peer, now);
        if (target == current || !target.HasPeer(peer)) return;
        current.Forget(peer, now);
        if (target == _engine) _routes.Remove(peer); else _routes[peer] = target;
    }

    private RelayEngine CarMeet(uint lobby)
    {
        if (!_carMeets.TryGetValue(lobby, out var engine)) _carMeets[lobby] = engine = NewEngine(lobby);
        return engine;
    }

    public ushort Port { get; }
    public bool Running => Volatile.Read(ref _running) != 0 && Volatile.Read(ref _disposed) == 0;
    public RelayNetworkStatus NetworkStatus => Volatile.Read(ref _network);

    /// <summary>Binds before returning, so a caller never advertises a world whose UDP port is occupied.</summary>
    public static IntegratedHostRelay Start(uint maxPlayers, bool races, ushort port = HostRelay.DefaultPort, IntegratedRelayOptions? options = null)
    {
        options ??= new();
        ArgumentNullException.ThrowIfNull(options.StunServers);
        if (options.StunInterval < TimeSpan.FromMilliseconds(50) || options.StunInterval > TimeSpan.FromHours(1))
            throw new ArgumentOutOfRangeException(nameof(options));
        var socket = RelayNetwork.Bind(port);
        try { return new(socket, maxPlayers, races, options); }
        catch { socket.Dispose(); throw; }
    }

    public Task<IReadOnlyList<string>> PeersAsync(CancellationToken cancellation)
    {
        cancellation.ThrowIfCancellationRequested();
        return Task.FromResult(Running ? Volatile.Read(ref _peers) : (IReadOnlyList<string>)Array.Empty<string>());
    }

    public void Kick(string xuid)
    {
        if (ulong.TryParse(xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var value))
            Enqueue(() => { foreach (var engine in Engines) engine.Kick(value, _clock.ElapsedMilliseconds); });
    }

    /// <summary>Queues a bounded STUN punch; neither caller nor WPF dispatcher performs socket I/O.</summary>
    public void Punch(IPEndPoint endpoint)
    {
        ArgumentNullException.ThrowIfNull(endpoint);
        var copy = new IPEndPoint(endpoint.Address, endpoint.Port);
        Enqueue(() => _nat.PunchPeer(copy, _clock.ElapsedMilliseconds));
    }

    /// <summary>Completes only after the actor installs the admission policy. Null is never allow-all.</summary>
    public Task SetAdmissionValidatorAsync(Func<ulong,IPEndPoint,long,bool>? validator,CancellationToken cancellation=default)
        => SetAdmissionPolicyAsync(validator,null,null,cancellation);

    internal async Task SetAdmissionPolicyAsync(Func<ulong, IPEndPoint, long, bool>? validator,
        Func<ulong,IPEndPoint,long,bool>? hookValidator,Func<ReadOnlyMemory<byte>,IPEndPoint,byte[]?>? proofReceiver,CancellationToken cancellation=default)
    {
        ObjectDisposedException.ThrowIf(!Running, this);
        var completion = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        using var bound = CancellationTokenSource.CreateLinkedTokenSource(cancellation, _stop.Token);
        bound.CancelAfter(TimeSpan.FromSeconds(2));
        var token = bound.Token;
        try
        {
            token.ThrowIfCancellationRequested();
            // Do not accumulate an unbounded queue of channel writers behind a busy actor.
            if (!_commands.Writer.TryWrite(new(() =>
                {
                    _authorize = validator ?? (static (_, _, _) => false); _authorizeHook = hookValidator; _proofReceiver = proofReceiver;
                    foreach (var engine in Engines) { engine.Authorize = _authorize; engine.AuthorizeHook = _authorizeHook; }
                }, completion, token)))
                throw new IOException("The relay control queue is full or closed");
            await completion.Task.WaitAsync(token).ConfigureAwait(false);
        }
        catch (ChannelClosedException) { throw new ObjectDisposedException(nameof(IntegratedHostRelay)); }
    }

    private bool Enqueue(Action action) => Running && _commands.Writer.TryWrite(new(action));
    internal void PunchNetworkHints(WorldRelayAdmission admission) => Enqueue(()=>
    {
        foreach(var hint in admission.NetworkHints())_nat.PunchGranted(hint.Grant,hint.Revision,hint.Endpoints,_clock.ElapsedMilliseconds);
    });

    private async Task DiscoverAsync(string[] names, CancellationToken cancellation)
    {
        var servers = await RelayNetworkNat.ResolveAsync(names, cancellation).ConfigureAwait(false);
        Enqueue(() => _nat.SetServers(servers));
    }

    /// <summary>CR-S57: the grants whose joiner meets this host at a relay (from the verified punches); the actor applies them.</summary>
    internal void SetRelayLegs(IReadOnlyList<RelayLegSpec> legs)
    {
        ArgumentNullException.ThrowIfNull(legs);
        var copy = legs.ToArray();
        Enqueue(() => _legs.Update(copy, _clock.ElapsedMilliseconds));
    }

    /// <summary>The engine's and the proof's datagrams: to a relayed joiner as <c>data</c> to its relay; never to a virtual address.</summary>
    private void Send(RelayDatagram datagram)
    {
        if (_legs.TryWrap(datagram.Destination, datagram.Data, out var wrapped)) { SendRaw(wrapped); return; }
        if (RelayLegs.IsVirtual(datagram.Destination)) return; // a relayed joiner whose leg is not confirmed (or gone)
        SendRaw(datagram);
    }

    private void SendRaw(RelayDatagram datagram)
    {
        try { _socket.SendTo(datagram.Data, SocketFlags.None, StunCodec.ForSocket(datagram.Destination, _socket.AddressFamily == AddressFamily.InterNetworkV6)); }
        catch (SocketException) { if (_sendErrors != uint.MaxValue) _sendErrors++; /* UDP loss is handled by reliability or the next bounded discovery round. */ }
        catch (ObjectDisposedException) { /* cancellation closed the socket */ }
    }

    private void Run(CancellationToken cancellation)
    {
        var buffer = new byte[65535];
        var nextTick = 0L;
        try
        {
            while (!cancellation.IsCancellationRequested)
            {
                var activity = false;
                for (var index = 0; index < 64 && _commands.Reader.TryRead(out var command); index++)
                {
                    activity = true;
                    if (command.Cancellation.IsCancellationRequested)
                    {
                        command.Completion?.TrySetCanceled(command.Cancellation);
                        continue;
                    }
                    try { command.Work(); command.Completion?.TrySetResult(); }
                    catch (Exception exception) { command.Completion?.TrySetException(exception); throw; }
                }
                for (var index = 0; index < 64 && _socket.Poll(0, SelectMode.SelectRead); index++)
                {
                    EndPoint from = _socket.AddressFamily == AddressFamily.InterNetworkV6 ? new IPEndPoint(IPAddress.IPv6Any, 0) : new IPEndPoint(IPAddress.Any, 0);
                    int count;
                    try { count = _socket.ReceiveFrom(buffer, SocketFlags.None, ref from); }
                    catch (SocketException exception) when (exception.SocketErrorCode is SocketError.WouldBlock or SocketError.ConnectionReset or SocketError.MessageSize) { break; }
                    var now = _clock.ElapsedMilliseconds;
                    var peer = StunCodec.Canonical((IPEndPoint)from);
                    var datagram = buffer.AsMemory(0, count);
                    // CR-S57: a relay's FIR1 datagram for one of this host's legs; its `data` is the joiner's datagram from a virtual peer
                    switch (_legs.Receive(datagram.Span, peer, now, out var relayed, out var payload))
                    {
                        case RelayLegs.Received.Handled: activity = true; continue;
                        case RelayLegs.Received.Data: peer = relayed!; datagram = payload; break;
                    }
                    if(datagram.Length>=4 && datagram.Span[..4].SequenceEqual("FIUP"u8))
                    {
                        if(_proofReceiver?.Invoke(datagram,peer) is { } proofReply)Send(new(peer,proofReply));
                        activity=true;continue;
                    }
                    if (!RelayLegs.IsVirtual(peer)) _nat.Receive(datagram.Span, peer, now);
                    // The engine ignores Binding messages and owns the STUN-framed hook side channels.
                    Route(datagram, peer, now);
                    activity = true;
                }
                var at = _clock.ElapsedMilliseconds;
                if (at >= nextTick)
                {
                    foreach (var engine in Engines) engine.Tick(at);
                    _nat.Tick(at); _legs.Tick(at); nextTick = at + 20;
                    // an endpoint whose car meet seat ended (dropped, timed out) is free roam's again
                    foreach (var gone in _routes.Where(p => !p.Value.HasPeer(p.Key)).Select(p => p.Key).ToArray()) _routes.Remove(gone);
                    Volatile.Write(ref _network, _nat.Snapshot() with { SendErrors = _sendErrors });
                    Volatile.Write(ref _peers, Array.AsReadOnly(Engines.SelectMany(e => e.PeerXuids).Distinct()
                        .Select(xuid => xuid.ToString(CultureInfo.InvariantCulture)).ToArray()));
                }
                if (!activity) cancellation.WaitHandle.WaitOne(2);
            }
        }
        catch (Exception exception) when (exception is SocketException or ObjectDisposedException or OperationCanceledException) { }
        finally
        {
            Volatile.Write(ref _running, 0);
            _stop.Cancel();
            _commands.Writer.TryComplete();
            while (_commands.Reader.TryRead(out var command)) command.Completion?.TrySetCanceled(cancellation);
            foreach (var engine in Engines) engine.Clear();
            _routes.Clear();
            Volatile.Write(ref _peers, Array.Empty<string>());
            _socket.Dispose();
        }
    }

    public void Dispose()
    {
        if (Interlocked.Exchange(ref _disposed, 1) != 0) return;
        _stop.Cancel();
        _commands.Writer.TryComplete();
        _socket.Dispose();
        // Shutdown is bounded even if a platform DNS/router operation ignores cancellation.
        var stopped = Task.WhenAll(_actor, _discovery, _upnp);
        try { stopped.Wait(TimeSpan.FromSeconds(2)); }
        catch (AggregateException) { /* cancellation/fault is represented by Running=false */ }
        if (stopped.IsCompleted) _stop.Dispose();
        else _ = stopped.ContinueWith(static (done, state) =>
        {
            _ = done.Exception;
            ((CancellationTokenSource)state!).Dispose();
        }, _stop, CancellationToken.None, TaskContinuationOptions.ExecuteSynchronously, TaskScheduler.Default);
    }
}

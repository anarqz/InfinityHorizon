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
    private readonly RelayEngine _engine;
    private readonly RelayNetworkNat _nat;
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

    private IntegratedHostRelay(Socket socket, uint maxPlayers, bool races, IntegratedRelayOptions options)
    {
        _socket = socket;
        Port = checked((ushort)((IPEndPoint)socket.LocalEndPoint!).Port);
        var dual = socket.AddressFamily == AddressFamily.InterNetworkV6;
        _nat = new(dual, (long)options.StunInterval.TotalMilliseconds, Send);
        if (options.EnableDiscovery && options.EnableUpnp) _nat.SetUpnp(RelayUpnpState.Pending);
        _network = _nat.Snapshot();
        // World admission must be installed before any game datagram can establish a seat.
        _engine = new(new RelaySettings { MaxPlayers = checked((int)Math.Clamp(maxPlayers, 1u, 12u)), Races = races }, Send)
        { Authorize = static (_, _, _) => false };
        var cancellation = _stop.Token;
        _actor = Task.Factory.StartNew(() => Run(cancellation), CancellationToken.None, TaskCreationOptions.LongRunning, TaskScheduler.Default);
        var serverNames = options.StunServers.Take(8).ToArray();
        _discovery = options.EnableDiscovery ? Task.Run(() => DiscoverAsync(serverNames, cancellation), cancellation) : Task.CompletedTask;
        _upnp = options.EnableDiscovery && options.EnableUpnp
            ? Task.Run(() => RelayNetworkUpnp.RunAsync(Port, mapping => Enqueue(() => _nat.SetUpnp(mapping.State, mapping.Address)), cancellation), cancellation) : Task.CompletedTask;
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
            Enqueue(() => _engine.Kick(value, _clock.ElapsedMilliseconds));
    }

    /// <summary>Queues a bounded STUN punch; neither caller nor WPF dispatcher performs socket I/O.</summary>
    public void Punch(IPEndPoint endpoint)
    {
        ArgumentNullException.ThrowIfNull(endpoint);
        var copy = new IPEndPoint(endpoint.Address, endpoint.Port);
        Enqueue(() => _nat.PunchPeer(copy, _clock.ElapsedMilliseconds));
    }

    /// <summary>Completes only after the actor installs the admission policy. Null is never allow-all.</summary>
    public async Task SetAdmissionValidatorAsync(Func<ulong, IPEndPoint, long, bool>? validator, CancellationToken cancellation = default)
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
            if (!_commands.Writer.TryWrite(new(() => _engine.Authorize = validator ?? (static (_, _, _) => false), completion, token)))
                throw new IOException("The relay control queue is full or closed");
            await completion.Task.WaitAsync(token).ConfigureAwait(false);
        }
        catch (ChannelClosedException) { throw new ObjectDisposedException(nameof(IntegratedHostRelay)); }
    }

    private bool Enqueue(Action action) => Running && _commands.Writer.TryWrite(new(action));

    private async Task DiscoverAsync(string[] names, CancellationToken cancellation)
    {
        var servers = await RelayNetworkNat.ResolveAsync(names, cancellation).ConfigureAwait(false);
        Enqueue(() => _nat.SetServers(servers));
    }

    private void Send(RelayDatagram datagram)
    {
        try { _socket.SendTo(datagram.Data, SocketFlags.None, StunCodec.ForSocket(datagram.Destination, _socket.AddressFamily == AddressFamily.InterNetworkV6)); }
        catch (SocketException) { /* UDP loss is handled by reliability or the next bounded discovery round. */ }
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
                    _nat.Receive(buffer.AsSpan(0, count), peer, now);
                    // The engine ignores Binding messages and owns the STUN-framed hook side channels.
                    _engine.Receive(buffer.AsSpan(0, count), peer, now);
                    activity = true;
                }
                var at = _clock.ElapsedMilliseconds;
                if (at >= nextTick)
                {
                    _engine.Tick(at); _nat.Tick(at); nextTick = at + 20;
                    Volatile.Write(ref _network, _nat.Snapshot());
                    Volatile.Write(ref _peers, Array.AsReadOnly(_engine.PeerXuids.Select(xuid => xuid.ToString(CultureInfo.InvariantCulture)).ToArray()));
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
            _engine.Clear();
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

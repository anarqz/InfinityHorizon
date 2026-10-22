// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net;
using System.Net.Sockets;
using System.Security.Cryptography;

namespace Infinity.Core.WorldServer;

/// <summary>Actor-owned STUN transactions, rate-limited answers and bounded peer punches.</summary>
/// <remarks>Between full rounds one IPv4 server per <paramref name="keepaliveMs"/> refreshes the relay's
/// mapping: routers whose UDP idle timeout is shorter than the round would otherwise hand the world a
/// new external port while it waits for players, leaving the announced one stale.</remarks>
internal sealed class RelayNetworkNat(bool dualStack, long intervalMs, Action<RelayDatagram> send, long keepaliveMs = 5000)
{
    private sealed record Pending(byte[] Transaction, IPEndPoint Server, long Sent);
    private sealed class Punch(IPEndPoint endpoint, long next)
    {
        internal IPEndPoint Endpoint { get; } = endpoint;
        internal byte[] Transaction { get; } = RandomNumberGenerator.GetBytes(12);
        internal int Left { get; set; } = 10;
        internal long Next { get; set; } = next;
    }

    private readonly List<Pending> _pending = [];
    private readonly List<(IPEndPoint Server, IPEndPoint Seen, long At)> _seen = [];
    private readonly List<Punch> _punches = [];
    private IPEndPoint[] _servers = [];
    private long _lastRound = -1, _answerAt, _lastKeepalive = -1;
    private int _answers, _keepaliveServer;
    private readonly Dictionary<Guid,(uint Revision,long Window,int Sent,long Seen)> _grantedPunches=[];
    internal void PunchGranted(Guid grant,uint revision,IReadOnlyList<IPEndPoint> endpoints,long now)
    {
        foreach(var old in _grantedPunches.Where(p=>now-p.Value.Seen>=90000).Select(p=>p.Key).ToArray())_grantedPunches.Remove(old);
        var previous=_grantedPunches.GetValueOrDefault(grant);
        if(revision<=previous.Revision || endpoints.Count>5)return;
        if(!_grantedPunches.ContainsKey(grant) && _grantedPunches.Count>=64)return;
        var window=previous.Window;var sent=previous.Sent;
        if(now-window>=15000) {window=now;sent=0;}
        foreach(var endpoint in endpoints)
        {
            if(sent+2>10)break;
            if(endpoint.AddressFamily==AddressFamily.InterNetworkV6 && !dualStack)continue;
            for(var i=0;i<2;i++)send(new(StunCodec.Canonical(endpoint),StunCodec.BindingRequest(RandomNumberGenerator.GetBytes(12))));
            sent+=2;
        }
        _grantedPunches[grant]=(revision,window,sent,now);
    }
    private RelayUpnpState _upnp = RelayUpnpState.Off;
    private IPEndPoint? _upnpAddress;

    internal void SetServers(IPEndPoint[] servers) => _servers = servers.Take(16).Select(StunCodec.Canonical)
        .Where(server => server.AddressFamily == AddressFamily.InterNetwork || dualStack).Distinct().ToArray();

    internal void SetUpnp(RelayUpnpState state, IPEndPoint? address = null) { _upnp = state; _upnpAddress = address; }

    internal void PunchPeer(IPEndPoint endpoint, long now)
    {
        endpoint = StunCodec.Canonical(endpoint);
        if (endpoint.Port == 0 || endpoint.Address.Equals(IPAddress.Any) || endpoint.Address.Equals(IPAddress.IPv6Any)
            || endpoint.Address.IsIPv6Multicast || (endpoint.AddressFamily == AddressFamily.InterNetwork && endpoint.Address.GetAddressBytes()[0] >= 224)
            || (!dualStack && endpoint.AddressFamily == AddressFamily.InterNetworkV6)) return;
        if (_punches.FirstOrDefault(p => p.Endpoint.Equals(endpoint)) is { } old) { old.Left = 10; old.Next = now; return; }
        if (_punches.Count == 32) _punches.RemoveAt(0);
        _punches.Add(new(endpoint, now));
    }

    internal bool Receive(ReadOnlySpan<byte> data, IPEndPoint from, long now)
    {
        if (!StunCodec.IsStun(data)) return false;
        var message = StunCodec.Parse(data);
        if (message is null) return true;
        from = StunCodec.Canonical(from);
        if (message.Kind == StunCodec.BindingRequestKind)
        {
            if (now - _answerAt >= 1000) { _answerAt = now; _answers = 0; }
            if (_answers++ < 50) send(new(from, StunCodec.BindingSuccess(message.Transaction, from)));
        }
        else if (message.Kind == StunCodec.BindingSuccessKind)
        {
            var at = _pending.FindIndex(p => p.Server.Equals(from) && p.Transaction.AsSpan().SequenceEqual(message.Transaction) && now - p.Sent < 10000);
            if (at >= 0)
            {
                _pending.RemoveAt(at);
                if (message.Mapped is { Port: > 0 } mapped)
                {
                    mapped = StunCodec.Canonical(mapped);
                    // A changed mapping re-measures every server now, so the NAT kind never mixes generations.
                    if (_seen.Any(p => p.Server.Equals(from) && !p.Seen.Equals(mapped)) && now - _lastRound >= 1000) _lastRound = -1;
                    _seen.RemoveAll(p => p.Server.Equals(from));
                    _seen.Add((from, mapped, now));
                }
            }
            else _punches.RemoveAll(p => p.Endpoint.Equals(from) && p.Transaction.AsSpan().SequenceEqual(message.Transaction));
        }
        return true;
    }

    internal void Tick(long now)
    {
        _pending.RemoveAll(p => now - p.Sent >= 10000);
        _seen.RemoveAll(p => now - p.At >= Math.Max(60000, intervalMs * 3));
        if (_servers.Length > 0 && (_lastRound < 0 || now - _lastRound >= intervalMs))
        {
            _lastRound = now;
            foreach (var server in _servers)
            {
                if (_pending.Count >= 16) break;
                var transaction = RandomNumberGenerator.GetBytes(12);
                _pending.Add(new(transaction, server, now));
                send(new(server, StunCodec.BindingRequest(transaction)));
            }
            _lastKeepalive = now;
        }
        else if (keepaliveMs > 0 && now - _lastKeepalive >= keepaliveMs
            && _servers.Where(s => s.AddressFamily == AddressFamily.InterNetwork).ToArray() is { Length: > 0 } ipv4 && _pending.Count < 16)
        {
            _lastKeepalive = now;
            _keepaliveServer = (_keepaliveServer + 1) % ipv4.Length;
            var server = ipv4[_keepaliveServer];
            var transaction = RandomNumberGenerator.GetBytes(12);
            _pending.Add(new(transaction, server, now));
            send(new(server, StunCodec.BindingRequest(transaction)));
        }
        foreach (var punch in _punches.Where(p => now >= p.Next))
        {
            send(new(punch.Endpoint, StunCodec.BindingRequest(punch.Transaction)));
            punch.Left--; punch.Next = now + 200;
        }
        _punches.RemoveAll(p => p.Left == 0);
    }

    internal RelayNetworkStatus Snapshot()
    {
        var ipv4 = _seen.Where(p => p.Server.AddressFamily == AddressFamily.InterNetwork).Select(p => p.Seen).ToArray();
        var kind = ipv4.Length < 2 ? RelayNatKind.Unknown : ipv4[0].Equals(ipv4[1]) ? RelayNatKind.Cone : RelayNatKind.Symmetric;
        var addresses = _seen.Select(p => p.Seen).DistinctBy(p => p.AddressFamily).Select(RelayNetwork.Value).ToArray();
        return new(kind, Array.AsReadOnly(addresses), _upnp, _upnpAddress is null ? null : RelayNetwork.Value(_upnpAddress),
            _upnpAddress is not null && RelayNetwork.BehindAnotherNat(_upnpAddress.Address), dualStack);
    }

    internal static async Task<IPEndPoint[]> ResolveAsync(IEnumerable<string> names, CancellationToken cancellation)
    {
        var resolved = new List<IPEndPoint>();
        foreach (var name in names.Take(8))
        {
            using var timeout = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
            timeout.CancelAfter(TimeSpan.FromSeconds(3));
            try
            {
                if (!Uri.TryCreate("udp://" + name, UriKind.Absolute, out var uri) || uri.Port is < 1 or > 65535) continue;
                var addresses = await Dns.GetHostAddressesAsync(uri.DnsSafeHost, timeout.Token).ConfigureAwait(false);
                foreach (var family in new[] { AddressFamily.InterNetwork, AddressFamily.InterNetworkV6 })
                    if (addresses.FirstOrDefault(ip => ip.AddressFamily == family) is { } address) resolved.Add(new(address, uri.Port));
            }
            catch (Exception exception) when (exception is SocketException or OperationCanceledException or ArgumentException)
            { if (cancellation.IsCancellationRequested) break; }
        }
        return resolved.ToArray();
    }
}

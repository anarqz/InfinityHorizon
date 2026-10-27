// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Buffers.Binary;
using System.Net;
using System.Net.Sockets;

namespace Infinity.Core.WorldServer;

/// <summary>
/// One grant the host meets its joiner for at a relay (CR-S57, MATCHMAKING_API.md §5b): the verified grant's exact claims bytes and
/// signature, the signed relay's rendezvous endpoint, and whether the pair's datagrams go through it now (the punch's transport).
/// </summary>
internal sealed record RelayLegSpec(Guid Grant, byte[] Claims, byte[] Signature, IPEndPoint Relay, bool Forward);

/// <summary>
/// The host's relay legs, the FIR1 rendezvous protocol of <c>fi_relay::wire</c> spoken from the world's own socket: a
/// <c>hello</c> (role host) as soon as a grant names a relay, again with the cookie of its <c>challenge</c> and every 15 s, and
/// <c>relay</c> (every second until <c>relay ok</c>) while the pair is relayed. Each relayed joiner is one virtual endpoint the
/// world engine sees as a peer: <c>data</c> from the relay arrives from it, and what the engine sends to it leaves as <c>data</c>.
/// Only the relay a verified grant names is ever contacted, and only its own datagrams are read. Single-threaded (the relay actor).
/// </summary>
internal sealed class RelayLegs(Action<RelayDatagram> send)
{
    internal static ReadOnlySpan<byte> Magic => "FIR1"u8;
    internal const int MaxPayload = 1200;
    internal const long HelloEveryMs = 15000;
    internal const long RelayEveryMs = 1000;
    internal const int MaxLegs = 64;

    /// <summary>A virtual peer's address: unique local IPv6 <c>fd46:4952::/32</c> ("FIR"), never routable, never sent to.</summary>
    private static readonly byte[] VirtualPrefix = [0xfd, 0x46, 0x49, 0x52];

    internal enum Received { NotOurs, Handled, Data }

    private sealed class Leg(RelayLegSpec spec)
    {
        public RelayLegSpec Spec { get; set; } = spec;
        public byte[] Id { get; } = spec.Grant.ToByteArray(bigEndian: true);
        public IPEndPoint Virtual { get; } = VirtualFor(spec.Grant);
        public byte[]? Cookie { get; set; }
        public long LastHello { get; set; } = long.MinValue;
        public long LastRelay { get; set; } = long.MinValue;
        public bool Ok { get; set; }
        public bool Unavailable { get; set; }
    }

    private readonly Dictionary<Guid, Leg> _legs = [];

    internal int Count => _legs.Count;

    internal static IPEndPoint VirtualFor(Guid grant)
    {
        var address = new byte[16];
        VirtualPrefix.CopyTo(address, 0);
        grant.ToByteArray(bigEndian: true).AsSpan(0, 12).CopyTo(address.AsSpan(4));
        return new(new IPAddress(address), 1);
    }

    internal static bool IsVirtual(IPEndPoint endpoint) => endpoint.AddressFamily == AddressFamily.InterNetworkV6
        && endpoint.Address.GetAddressBytes().AsSpan(0, 4).SequenceEqual(VirtualPrefix);

    /// <summary>Whether the pair of <paramref name="grant"/> is carried by its relay now (<c>relay ok</c> received).</summary>
    internal bool Relayed(Guid grant) => _legs.TryGetValue(grant, out var leg) && leg.Ok;

    /// <summary>The grants that name a relay now; legs no longer listed are dropped, new ones say hello at once.</summary>
    internal void Update(IReadOnlyList<RelayLegSpec> specs, long now)
    {
        ArgumentNullException.ThrowIfNull(specs);
        var wanted = specs.Where(s => s.Relay.Port > 0).DistinctBy(s => s.Grant).Take(MaxLegs).ToDictionary(s => s.Grant);
        foreach (var gone in _legs.Keys.Where(g => !wanted.ContainsKey(g)).ToArray()) _legs.Remove(gone);
        foreach (var (grant, spec) in wanted)
        {
            if (_legs.TryGetValue(grant, out var leg) && leg.Spec.Relay.Equals(spec.Relay))
            {
                leg.Spec = spec; // re-signed copies keep the same grant and relay (the cookie stays valid for this address)
                continue;
            }
            _legs[grant] = new Leg(spec);
        }
        Tick(now);
    }

    internal void Tick(long now)
    {
        foreach (var leg in _legs.Values)
        {
            if (leg.LastHello == long.MinValue || now - leg.LastHello >= HelloEveryMs) Hello(leg, now);
            if (leg.Spec.Forward && !leg.Ok && !leg.Unavailable && (leg.LastRelay == long.MinValue || now - leg.LastRelay >= RelayEveryMs))
            {
                leg.LastRelay = now;
                send(new(leg.Spec.Relay, Frame(0x03, leg.Id)));
            }
        }
    }

    private void Hello(Leg leg, long now)
    {
        leg.LastHello = now;
        var claims = leg.Spec.Claims;
        if (claims.Length > ushort.MaxValue || leg.Spec.Signature.Length != 64) return;
        var body = new byte[1 + 2 + claims.Length + 64 + (leg.Cookie?.Length ?? 0)];
        body[0] = 2; // host
        BinaryPrimitives.WriteUInt16BigEndian(body.AsSpan(1), (ushort)claims.Length);
        claims.CopyTo(body, 3);
        leg.Spec.Signature.CopyTo(body, 3 + claims.Length);
        leg.Cookie?.CopyTo(body, 3 + claims.Length + 64);
        send(new(leg.Spec.Relay, Frame(0x01, body)));
    }

    /// <summary>
    /// A datagram from <paramref name="from"/>: <see cref="Received.NotOurs"/> unless it is FIR1 from one leg's relay about that leg's
    /// grant; <see cref="Received.Data"/> yields the joiner's payload and the virtual endpoint it comes from.
    /// </summary>
    internal Received Receive(ReadOnlySpan<byte> data, IPEndPoint from, long now, out IPEndPoint? peer, out byte[]? payload)
    {
        peer = null; payload = null;
        if (data.Length < 5 + 16 || !data[..4].SequenceEqual(Magic)) return Received.NotOurs;
        var kind = data[4];
        var body = data[5..];
        Leg? leg = null;
        foreach (var candidate in _legs.Values)
            if (candidate.Spec.Relay.Equals(from) && body[..16].SequenceEqual(candidate.Id)) { leg = candidate; break; }
        if (leg is null) return Received.NotOurs;
        switch (kind)
        {
            case 0x07 when body.Length == 32: // challenge: say hello again with its cookie
                leg.Cookie = body[16..32].ToArray();
                Hello(leg, now);
                return Received.Handled;
            case 0x04 when body.Length == 16: // relay ok
                leg.Ok = true; leg.Unavailable = false;
                return Received.Handled;
            case 0x06 when body.Length == 16: // relay unavailable: the joiner's own retries decide; keep the leg and its hellos
                leg.Ok = false; leg.Unavailable = true;
                return Received.Handled;
            case 0x02: // peer: the other side's public endpoint (informational; datagrams still go through the relay)
                return Received.Handled;
            case 0x05 when body.Length > 16 && body.Length - 16 <= MaxPayload:
                if (!leg.Ok) return Received.Handled; // forwarded data only after the relay confirmed this grant
                peer = leg.Virtual;
                payload = body[16..].ToArray();
                return Received.Data;
            default:
                return Received.Handled;
        }
    }

    /// <summary>The engine's datagram to a relayed joiner, as <c>data</c> to its relay; false for any other destination.</summary>
    internal bool TryWrap(IPEndPoint destination, ReadOnlySpan<byte> payload, out RelayDatagram wrapped)
    {
        wrapped = default!;
        if (!IsVirtual(destination) || payload.Length is 0 or > MaxPayload) return false;
        var leg = _legs.Values.FirstOrDefault(l => l.Virtual.Equals(destination));
        if (leg is not { Ok: true }) return false;
        var body = new byte[16 + payload.Length];
        leg.Id.CopyTo(body, 0);
        payload.CopyTo(body.AsSpan(16));
        wrapped = new(leg.Spec.Relay, Frame(0x05, body));
        return true;
    }

    private static byte[] Frame(byte kind, ReadOnlySpan<byte> body)
    {
        var frame = new byte[5 + body.Length];
        Magic.CopyTo(frame);
        frame[4] = kind;
        body.CopyTo(frame.AsSpan(5));
        return frame;
    }
}

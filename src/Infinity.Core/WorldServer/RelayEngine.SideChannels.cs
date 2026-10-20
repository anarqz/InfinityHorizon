// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Buffers.Binary;
using System.Net;
using System.Net.Sockets;

namespace Infinity.Core.WorldServer;

internal sealed partial class RelayEngine
{
    private readonly LiveryBoard _liveries = new();
    internal const int SideChannelPacketsPerSecond = 4096;
    internal const int SideChannelBytesPerSecond = 2 * 1024 * 1024;
    internal const int SideChannelControlsPerSecond = 128;
    private sealed class SideChannelRate(long now)
    {
        internal long At { get; set; } = now;
        internal double Packets { get; set; } = SideChannelPacketsPerSecond * 2.0;
        internal double Bytes { get; set; } = SideChannelBytesPerSecond * 2.0;
        internal double Controls { get; set; } = SideChannelControlsPerSecond * 2.0;
    }
    private readonly Dictionary<RelayPeer, SideChannelRate> _sideChannelRates = [];
    internal long SideChannelRateDrops { get; private set; }
    private long _lastLiveryTick;

    private bool TakeSideChannelRate(RelayPeer peer, int bytes, bool control)
    {
        if (!_sideChannelRates.TryGetValue(peer, out var rate)) _sideChannelRates[peer] = rate = new(_now);
        var elapsed = Math.Max(0, _now - rate.At) / 1000.0; rate.At = _now;
        rate.Packets = Math.Min(SideChannelPacketsPerSecond * 2.0, rate.Packets + elapsed * SideChannelPacketsPerSecond);
        rate.Bytes = Math.Min(SideChannelBytesPerSecond * 2.0, rate.Bytes + elapsed * SideChannelBytesPerSecond);
        rate.Controls = Math.Min(SideChannelControlsPerSecond * 2.0, rate.Controls + elapsed * SideChannelControlsPerSecond);
        if (rate.Packets < 1 || rate.Bytes < bytes || control && rate.Controls < 1)
        { SideChannelRateDrops++; return false; }
        rate.Packets--; rate.Bytes -= bytes;
        if (control) rate.Controls--;
        return true;
    }

    private bool MayUseLivery(ulong xuid) => _peers.Any(peer => peer.Synced && peer.Xuid == xuid
        && Allowed(peer.Xuid, peer.Endpoint) && (peer.LiveryHook is null || Allowed(peer.Xuid, peer.LiveryHook)));

    // A separate hook socket may bind only to a private peer on the same IP. Public NAT addresses
    // can represent unrelated players and therefore always require an exact endpoint.
    internal static bool MaySpeakFor(IPEndPoint peer, IPEndPoint from)
    {
        peer = StunCodec.Canonical(peer);
        from = StunCodec.Canonical(from);
        return peer.Equals(from) || IsPrivate(peer.Address) && peer.Address.Equals(from.Address);
    }

    private static bool IsPrivate(IPAddress address)
    {
        if (address.IsIPv4MappedToIPv6) address = address.MapToIPv4();
        if (IPAddress.IsLoopback(address)) return true;
        var bytes = address.GetAddressBytes();
        return address.AddressFamily == AddressFamily.InterNetwork
            ? bytes[0] == 10 || bytes[0] == 172 && bytes[1] is >= 16 and <= 31 || bytes[0] == 192 && bytes[1] == 168 || bytes[0] == 169 && bytes[1] == 254
            : (bytes[0] & 0xfe) == 0xfc || bytes[0] == 0xfe && (bytes[1] & 0xc0) == 0x80;
    }

    private void ReceiveSideChannel(ReadOnlySpan<byte> data, IPEndPoint endpoint)
    {
        switch (BinaryPrimitives.ReadUInt16BigEndian(data))
        {
            case CarStateCodec.Kind: ReceiveCarState(data, endpoint); break;
            case LiveryCodec.Kind: ReceiveLivery(data, endpoint); break;
        }
    }

    private void ReceiveCarState(ReadOnlySpan<byte> data, IPEndPoint endpoint)
    {
        if (CarStateCodec.ParseUp(data) is not { } up) return;
        var peer = _peers.FirstOrDefault(p => p.Endpoint.Equals(endpoint));
        if (peer is not null && peer.Xuid != up.Xuid) return;
        peer ??= _peers.FirstOrDefault(p => p.Synced && p.Xuid == up.Xuid && MaySpeakFor(p.Endpoint, endpoint));
        if (peer is null || !peer.Synced || !Allowed(peer.Xuid, peer.Endpoint) || !Allowed(peer.Xuid, endpoint)
            || !TakeSideChannelRate(peer, data.Length, true) || peer.CarStateAt is { } at && _now - at < 50) return;
        peer.CarStateAt = _now;
        peer.LastSeen = _now;
        peer.CarHook = endpoint;
        var changed = peer.CarState is not { } previous || previous.Lights != up.State.Lights || previous.Roof != up.State.Roof;
        peer.CarState = up.State;
        if (changed) PassCarState(peer);
    }

    private void PassCarState(RelayPeer from)
    {
        if (from.CarState is not { } state || !Allowed(from.Xuid, from.Endpoint)
            || from.CarHook is not { } source || !Allowed(from.Xuid, source)) return;
        var packet = CarStateCodec.WriteDown(new(state, from.Index, from.Xuid, from.CarPosition));
        foreach (var peer in _peers)
            if (peer != from && peer.Synced && Allowed(peer.Xuid, peer.Endpoint) && peer.CarHook is { } hook && Allowed(peer.Xuid, hook)) Emit(hook, packet);
    }

    private void ReceiveLivery(ReadOnlySpan<byte> data, IPEndPoint endpoint)
    {
        // Identify a known participant and apply its shared socket-independent work budget
        // before Parse allocates a chunk. Rotating source ports cannot reset this allowance.
        if (!StunCodec.TryAttribute(data, LiveryCodec.Kind, LiveryCodec.Attribute, out var value)
            || value.Length < 2 || value[0] != 1) return;
        var operation = value[1];
        var claimed = operation switch
        {
            1 when value.Length >= 34 => (ulong?)BinaryPrimitives.ReadUInt64BigEndian(value[16..]),
            3 when value.Length == 10 => BinaryPrimitives.ReadUInt64BigEndian(value[2..]),
            _ => null,
        };
        RelayPeer? peer;
        if (operation == 5 && value.Length == 12)
            peer = _peers.FirstOrDefault(p => p.Synced && p.LiveryHook?.Equals(endpoint) == true);
        else if (claimed is { } xuid)
        {
            peer = _peers.FirstOrDefault(p => p.Endpoint.Equals(endpoint));
            if (peer is not null && (!peer.Synced || peer.Xuid != xuid)) return;
            peer ??= _peers.FirstOrDefault(p => p.Synced && p.Xuid == xuid && MaySpeakFor(p.Endpoint, endpoint));
        }
        else return;
        if (peer is null || !Allowed(peer.Xuid, peer.Endpoint) || !Allowed(peer.Xuid, endpoint)
            || !TakeSideChannelRate(peer, data.Length, operation != 1)) return;
        if (LiveryCodec.Parse(data) is not { } message) { Rejected(peer, "invalid_livery"); return; }
        peer.LastSeen = _now;
        if (claimed.HasValue) peer.LiveryHook = endpoint;
        _liveries.Authorize = MayUseLivery;
        SendLiveries(message switch
        {
            LiveryMessage.Put put => _liveries.OnPut(peer.Xuid, put.Chunk, _now),
            LiveryMessage.Want => _liveries.OnWant(peer.Xuid, _now),
            LiveryMessage.Ack ack => _liveries.OnAck(peer.Xuid, ack.Owner, ack.Sequence),
            _ => []
        });
    }

    private void SendLiveries(IReadOnlyList<LiveryOutput> output)
    {
        foreach (var item in output)
        {
            if (item.Message is LiveryMessage.Data data && !MayUseLivery(data.Chunk.Owner)) continue;
            if (_peers.FirstOrDefault(p => p.Synced && p.Xuid == item.To && Allowed(p.Xuid, p.Endpoint)) is { LiveryHook: { } hook } peer
                && Allowed(peer.Xuid, hook))
                Emit(hook, LiveryCodec.Write(item.Message));
        }
        // Notes contain only IDs, reasons and sizes. This owner deliberately retains no log or history.
        _liveries.TakeNotes();
    }

    private void TickSideChannels()
    {
        _liveries.Authorize = MayUseLivery;
        if (_now - _lastLiveryTick >= LiveryBoard.OutputWindowMs)
        {
            _lastLiveryTick = _now;
            SendLiveries(_liveries.Tick(_now));
        }
        if (_now - _lastCarStates >= 1000)
        {
            _lastCarStates = _now;
            foreach (var peer in _peers) PassCarState(peer);
        }
    }

    private void OnPeerLeft(RelayPeer peer)
    {
        _sideChannelRates.Remove(peer);
        if (!_peers.Any(p => p.Xuid == peer.Xuid)) SendLiveries(_liveries.OnLeave(peer.Xuid));
        if (_peers.Count == 0) _liveries.Clear();
    }

    private void ClearSideChannels() { _liveries.Clear(); _sideChannelRates.Clear(); SideChannelRateDrops = 0; }
}
